//! HTTP client helpers for the agent-server API.
//!
//! Uses `ureq` (v3) to communicate with the server.

use tungstenite::stream::MaybeTlsStream;
use ureq::http;

use agent_core::types::TreeMeta;

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

impl From<tungstenite::Error> for ClientError {
    fn from(e: tungstenite::Error) -> Self {
        ClientError::Ws(e.to_string())
    }
}

impl From<ureq::Error> for ClientError {
    fn from(e: ureq::Error) -> Self {
        ClientError::Http(e.to_string())
    }
}

pub type ClientResult<T> = std::result::Result<T, ClientError>;

/// Build a base URL from the server address string.
fn base_url(server: &str) -> String {
    if server.starts_with("http://") || server.starts_with("https://") {
        server.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", server.trim_end_matches('/'))
    }
}

/// Extract error string from a response with status >= 400.
fn extract_error(resp: http::Response<ureq::Body>) -> ClientError {
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .read_to_string()
        .unwrap_or_else(|_| "unknown".to_string());
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(msg) = json.get("error").and_then(|v| v.as_str()) {
            return ClientError::Http(format!("HTTP {}: {}", status, msg));
        }
    }
    ClientError::Http(format!("HTTP {}: {}", status, body.lines().next().unwrap_or("")))
}

/// HTTP client for the agent-server.
#[derive(Clone)]
pub struct AgentClient {
    base: String,
    server: String,
}

impl AgentClient {
    pub fn new(server: &str) -> Self {
        Self {
            base: base_url(server),
            server: server.to_string(),
        }
    }

    pub fn server_addr(&self) -> &str {
        &self.server
    }

    fn get_json<T: for<'a> serde::Deserialize<'a>>(&self, path: &str) -> ClientResult<T> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::get(&url).call()?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        Ok(resp.into_body().read_json()?)
    }

    fn post_json<T: for<'a> serde::Deserialize<'a>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> ClientResult<T> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::post(&url).send_json(body)?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        Ok(resp.into_body().read_json()?)
    }

    fn post_empty(&self, path: &str) -> ClientResult<()> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::post(&url).send_json(serde_json::json!({}))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        Ok(())
    }

    pub fn list_trees(&self) -> ClientResult<Vec<TreeMeta>> {
        self.get_json("/trees")
    }

    pub fn create_tree(
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
            body.insert("repo_path".into(), serde_json::Value::String(p.to_string()));
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
    }

    pub fn get_tree(&self, id: &str) -> ClientResult<TreeMeta> {
        self.get_json(&format!("/trees/{}", id))
    }

    pub fn stop_agent(&self, tree_id: &str) -> ClientResult<()> {
        self.post_empty(&format!("/trees/{}/stop", tree_id))
    }
}

// ── WebSocket session for agent communication ──

pub fn parse_host_port(server: &str) -> ClientResult<(String, u16)> {
    let s = server.strip_prefix("http://").or_else(|| server.strip_prefix("https://")).unwrap_or(server);
    let s = s.trim_end_matches('/');
    let (host, port_str) = s.split_once(':')
        .ok_or_else(|| ClientError::Other(format!("invalid server address '{}': expected host:port", server)))?;
    let port: u16 = port_str.parse()
        .map_err(|e| ClientError::Other(format!("invalid port '{}': {}", port_str, e)))?;
    Ok((host.to_string(), port))
}

pub struct AgentSession {
    ws: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

pub enum TryEvent {
    Event(agent_core::types::ServerEvent),
    WouldBlock,
    Closed,
    Err(String),
}

impl AgentSession {
    pub fn connect(server: &str, tree_id: &str) -> ClientResult<Self> {
        let (host, port) = parse_host_port(server)?;
        let url = format!("ws://{}:{}/trees/{}/ws", host, port, tree_id);
        let (ws, _resp) = tungstenite::connect(url)?;
        Ok(Self { ws })
    }

    pub fn from_stream(
        stream: std::net::TcpStream,
        tree_id: &str,
    ) -> ClientResult<Self> {
        let url = format!("ws://localhost/trees/{}/ws", tree_id);
        let stream = tungstenite::stream::MaybeTlsStream::Plain(stream);
        let (ws, _) = tungstenite::client(url, stream)
            .map_err(|e| ClientError::Ws(e.to_string()))?;
        Ok(Self { ws })
    }

    pub fn set_nonblocking(&mut self, nb: bool) -> Result<(), String> {
        match self.ws.get_mut() {
            MaybeTlsStream::Plain(tcp) => {
                tcp.set_nonblocking(nb).map_err(|e| e.to_string())
            }
            _ => Err("Cannot set non-blocking on TLS stream".into()),
        }
    }

    pub fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        match self.ws.get_ref() {
            MaybeTlsStream::Plain(tcp) => Some(tcp.as_raw_fd()),
            _ => None,
        }
    }

    pub fn send_message(&mut self, text: &str) -> ClientResult<()> {
        let cmd = agent_core::rpc::WsCommand::Message {
            params: agent_core::rpc::MessageParams { text: text.into() },
        };
        let s = serde_json::to_string(&cmd)?;
        self.ws.send(tungstenite::Message::Text(s))?;
        Ok(())
    }

    pub fn send_stop(&mut self) -> ClientResult<()> {
        let s = serde_json::to_string(&agent_core::rpc::WsCommand::Stop)?;
        self.ws.send(tungstenite::Message::Text(s))?;
        Ok(())
    }

    pub fn try_next_event(&mut self) -> TryEvent {
        match self.ws.read() {
            Ok(tungstenite::Message::Text(s)) => {
                match serde_json::from_str(&s) {
                    Ok(ev) => TryEvent::Event(ev),
                    Err(e) => TryEvent::Err(format!("parse error: {}", e)),
                }
            }
            Ok(tungstenite::Message::Ping(p)) => {
                let _ = self.ws.send(tungstenite::Message::Pong(p));
                TryEvent::WouldBlock
            }
            Ok(tungstenite::Message::Close(_)) => TryEvent::Closed,
            Ok(_) => TryEvent::WouldBlock,
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => TryEvent::WouldBlock,
            Err(_) => TryEvent::Closed,
        }
    }

    pub fn next_event(&mut self) -> Option<Result<agent_core::types::ServerEvent, String>> {
        loop {
            match self.ws.read() {
                Ok(tungstenite::Message::Text(s)) => {
                    return Some(serde_json::from_str(&s).map_err(|e| e.to_string()));
                }
                Ok(tungstenite::Message::Ping(p)) => {
                    let _ = self.ws.send(tungstenite::Message::Pong(p));
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => return None,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base_url_formatting() {
        assert_eq!(base_url("localhost:8080"), "http://localhost:8080");
        assert_eq!(base_url("http://localhost:8080"), "http://localhost:8080");
        assert_eq!(base_url("http://localhost:8080/"), "http://localhost:8080");
        assert_eq!(base_url("127.0.0.1:9090"), "http://127.0.0.1:9090");
    }

    #[test]
    fn test_parse_host_port() {
        let (h, p) = parse_host_port("localhost:8080").unwrap();
        assert_eq!(h, "localhost");
        assert_eq!(p, 8080);

        let (h, p) = parse_host_port("http://localhost:8080").unwrap();
        assert_eq!(h, "localhost");
        assert_eq!(p, 8080);

        let (h, p) = parse_host_port("http://192.168.1.5:9090").unwrap();
        assert_eq!(h, "192.168.1.5");
        assert_eq!(p, 9090);
    }

    #[test]
    fn test_parse_host_port_bad_format() {
        assert!(parse_host_port("localhost").is_err());
        assert!(parse_host_port("localhost:abc").is_err());
    }

    #[test]
    fn test_agent_session_url_format() {
        let (host, port) = parse_host_port("localhost:8080").unwrap();
        let url = format!("ws://{}:{}/trees/abc123/ws", host, port);
        assert_eq!(url, "ws://localhost:8080/trees/abc123/ws");
    }

    #[test]
    fn test_wscommand_stop_serializes() {
        let s = serde_json::to_string(&agent_core::rpc::WsCommand::Stop).unwrap();
        assert_eq!(s, r#"{"method":"stop"}"#);
    }
}
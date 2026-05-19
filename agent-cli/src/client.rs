//! HTTP client helpers for the agent-server API.
//!
//! Uses `ureq` (v3) to communicate with the server.

use ureq::http;

use agent_core::types::{Entry, TreeMeta};

/// Build a base URL from the server address string.
fn base_url(server: &str) -> String {
    if server.starts_with("http://") || server.starts_with("https://") {
        server.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", server.trim_end_matches('/'))
    }
}

/// Extract error string from a response with status >= 400.
fn extract_error(resp: http::Response<ureq::Body>) -> String {
    let status = resp.status().as_u16();
    // Try reading the body for a JSON error message
    let body = resp
        .into_body()
        .read_to_string()
        .unwrap_or_else(|_| "unknown".to_string());
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(msg) = json.get("error").and_then(|v| v.as_str()) {
            return format!("HTTP {}: {}", status, msg);
        }
    }
    format!("HTTP {}: {}", status, body.lines().next().unwrap_or(""))
}

/// HTTP client for the agent-server.
#[derive(Clone)]
pub struct AgentClient {
    base: String,
}

impl AgentClient {
    /// Create a new client connected to the given server address.
    pub fn new(server: &str) -> Self {
        Self {
            base: base_url(server),
        }
    }

    fn get_json<T: for<'a> serde::Deserialize<'a>>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("request failed: {}", e))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        resp.into_body()
            .read_json()
            .map_err(|e| format!("failed to parse response: {}", e))
    }

    fn post_json<T: for<'a> serde::Deserialize<'a>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, String> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::post(&url)
            .send_json(body)
            .map_err(|e| format!("request failed: {}", e))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        resp.into_body()
            .read_json()
            .map_err(|e| format!("failed to parse response: {}", e))
    }

    fn post_empty(&self, path: &str) -> Result<(), String> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::post(&url)
            .send_json(&serde_json::json!({}))
            .map_err(|e| format!("request failed: {}", e))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        Ok(())
    }

    // ── Public API ──

    /// List all trees from the server.
    pub fn list_trees(&self) -> Result<Vec<TreeMeta>, String> {
        self.get_json("/trees")
    }

    /// Create a new tree with optional title, repo_path, and model.
    pub fn create_tree(
        &self,
        title: Option<&str>,
        repo_path: Option<&str>,
        model: Option<&str>,
    ) -> Result<TreeMeta, String> {
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
        self.post_json("/trees", &serde_json::Value::Object(body))
    }

    /// Get a single tree by ID.
    pub fn get_tree(&self, id: &str) -> Result<TreeMeta, String> {
        self.get_json(&format!("/trees/{}", id))
    }

    /// Stop the active agent for a tree.
    pub fn stop_agent(&self, tree_id: &str) -> Result<(), String> {
        self.post_empty(&format!("/trees/{}/stop", tree_id))
    }

    /// Get all entries for a tree.
    pub fn get_entries(&self, tree_id: &str) -> Result<Vec<Entry>, String> {
        self.get_json(&format!("/trees/{}", tree_id))
    }

    /// Ask the server to auto-generate a title for a tree.
    pub fn auto_title(&self, tree_id: &str) -> Result<String, String> {
        let url = format!("{}/trees/{}/auto-title", self.base, tree_id);
        let resp = ureq::post(&url)
            .send_json(&serde_json::json!({}))
            .map_err(|e| format!("request failed: {}", e))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        let json: serde_json::Value = resp
            .into_body()
            .read_json()
            .map_err(|e| format!("failed to parse response: {}", e))?;
        json.get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "no title in response".to_string())
    }
}

// ── WebSocket session for agent communication ──

/// Parse host and port from a server string like "localhost:8080" or "http://localhost:8080".
fn parse_host_port(server: &str) -> Result<(String, u16), String> {
    let s = server.strip_prefix("http://").or_else(|| server.strip_prefix("https://")).unwrap_or(server);
    let s = s.trim_end_matches('/');
    let (host, port_str) = s.split_once(':').ok_or_else(|| format!("invalid server address '{}': expected host:port", server))?;
    let port: u16 = port_str.parse().map_err(|e| format!("invalid port '{}': {}", port_str, e))?;
    Ok((host.to_string(), port))
}

/// WebSocket session to an agent worker.
pub struct AgentSession {
    ws: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl AgentSession {
    /// Connect to a tree's WebSocket session.
    pub fn connect(server: &str, tree_id: &str) -> Result<Self, String> {
        let (host, port) = parse_host_port(server)?;
        let url = format!("ws://{}:{}/trees/{}/ws", host, port, tree_id);
        let (ws, _resp) = tungstenite::connect(url).map_err(|e| format!("WS connect failed: {}", e))?;
        Ok(Self { ws })
    }

    /// Send a user message to the agent.
    pub fn send_message(&mut self, text: &str) -> Result<(), String> {
        let cmd = agent_core::rpc::WsCommand::Message {
            params: agent_core::rpc::MessageParams { text: text.into() },
        };
        let s = serde_json::to_string(&cmd).map_err(|e| e.to_string())?;
        self.ws.send(tungstenite::Message::Text(s)).map_err(|e| e.to_string())
    }

    /// Send a stop command to the agent.
    pub fn send_stop(&mut self) -> Result<(), String> {
        let s = serde_json::to_string(&agent_core::rpc::WsCommand::Stop).map_err(|e| e.to_string())?;
        self.ws.send(tungstenite::Message::Text(s)).map_err(|e| e.to_string())
    }

    /// Read the next event from the WebSocket. Returns None on close/error.
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
}
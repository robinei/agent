//! HTTP client helpers for the agent-server API.
//!
//! Uses `ureq` (v3) to communicate with the server.

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::time::Duration;

use ureq::http;

use agent_core::types::{Entry, ServerEvent, TreeMeta};

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

    /// Send a user message to an active (or auto-spawned) agent.
    pub fn send_message(&self, tree_id: &str, text: &str) -> Result<(), String> {
        let body = serde_json::json!({ "text": text });
        let url = format!("{}/trees/{}/message", self.base, tree_id);
        let resp = ureq::post(&url)
            .send_json(&body)
            .map_err(|e| format!("request failed: {}", e))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        Ok(())
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

    /// Open an SSE event stream for an active agent.
    pub fn stream_events(&self, tree_id: &str) -> Result<SseEventStream, String> {
        let url = format!("{}/trees/{}/stream", self.base, tree_id);
        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("request failed: {}", e))?;
        if resp.status().as_u16() >= 400 {
            return Err(extract_error(resp));
        }
        Ok(SseEventStream::new(resp.into_body().into_reader()))
    }
}

/// Background-read SSE events and deliver via channel for pollable access.
pub struct SseEventStream {
    rx: mpsc::Receiver<Option<ServerEvent>>,
    poll_timeout: Duration,
}

impl SseEventStream {
    /// Spawn a reader thread and return a pollable stream.
    pub fn new(reader: ureq::BodyReader<'static>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("sse-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => { let _ = tx.send(None); break; }
                        Ok(_) => {}
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    let data = trimmed.strip_prefix("data: ").unwrap_or(trimmed);
                    if data == "[DONE]" { let _ = tx.send(None); break; }
                    match serde_json::from_str::<ServerEvent>(data) {
                        Ok(event) => { if tx.send(Some(event)).is_err() { break; } }
                        Err(e) => eprintln!("[client] Warning: failed to parse SSE event: {}", e),
                    }
                }
            })
            .ok();
        Self { rx, poll_timeout: Duration::from_millis(200) }
    }

    /// Block until the next event arrives.
    pub fn next_event(&mut self) -> Option<ServerEvent> {
        self.rx.recv().ok()?
    }

    /// Return the next event, or None after `poll_timeout`.
    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.rx.recv_timeout(self.poll_timeout).ok()?
    }
}

// ── Tests ──

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
}
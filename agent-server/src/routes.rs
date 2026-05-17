use std::borrow::Cow;
use std::io::{self, Read};
use std::sync::Arc;

use rouille::{router, Request, Response, ResponseBody};
use serde::Deserialize;

use agent_core::store::Store;
use agent_core::types::{Entry, ServerEvent, TreeMeta};

use crate::lifecycle;

// ── Request body types ──

#[derive(Deserialize)]
pub struct CreateTreeBody {
    pub title: Option<String>,
    pub repo_path: Option<String>,
    pub model: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateTreeBody {
    pub title: Option<String>,
}

#[derive(Deserialize)]
pub struct SendMessageBody {
    pub text: String,
}

// ── Route handler ──

pub fn handle_request(request: &Request, store: &Arc<Store>) -> Response {
    router!(request,
        (GET) (/) => {
            Response::json(&serde_json::json!({
                "service": "agent-server",
                "version": "0.1.0",
            }))
        },

        (GET) (/trees) => {
            handle_list_trees(store)
        },

        (POST) (/trees) => {
            handle_create_tree(request, store)
        },

        (GET) (/trees/{id: String}) => {
            handle_get_tree(&id, store)
        },

        (PATCH) (/trees/{id: String}) => {
            handle_update_tree(&id, request, store)
        },

        (GET) (/trees/{id: String}/entries) => {
            handle_list_entries(&id, store)
        },

        (POST) (/trees/{id: String}/message) => {
            handle_send_message(&id, request)
        },

        (POST) (/trees/{id: String}/stop) => {
            handle_stop_agent(&id)
        },

        (GET) (/trees/{id: String}/stream) => {
            handle_sse_stream(&id)
        },

        _ => {
            Response::json(&serde_json::json!({"error": "not found"}))
                .with_status_code(404)
        }
    )
}

// ── SSE Streaming ──

/// A Read implementation that serves SSE events with reconnection support.
pub struct SseReconnectStream {
    catch_up: std::vec::IntoIter<ServerEvent>,
    rx: std::sync::mpsc::Receiver<ServerEvent>,
    buf: Vec<u8>,
    pos: usize,
}

impl SseReconnectStream {
    pub fn new(
        catch_up: Vec<ServerEvent>,
        rx: std::sync::mpsc::Receiver<ServerEvent>,
    ) -> Self {
        Self {
            catch_up: catch_up.into_iter(),
            rx,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for SseReconnectStream {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            let event = if let Some(e) = self.catch_up.next() {
                e
            } else {
                match self.rx.recv() {
                    Ok(e) => e,
                    Err(_) => return Ok(0),
                }
            };
            self.buf = format!(
                "data: {}\n\n",
                serde_json::to_string(&event).unwrap()
            )
            .into_bytes();
            self.pos = 0;
        }
        let n = (&self.buf[self.pos..]).read(dst)?;
        self.pos += n;
        Ok(n)
    }
}

// ── Handlers ──

/// GET /trees — list all trees
fn handle_list_trees(store: &Store) -> Response {
    match store.list_trees() {
        Ok(trees) => Response::json(&trees),
        Err(e) => Response::json(&serde_json::json!({"error": format!("{}", e)}))
            .with_status_code(500),
    }
}

/// POST /trees — create a new tree
fn handle_create_tree(request: &Request, store: &Store) -> Response {
    let body: CreateTreeBody = match request
        .data()
        .and_then(|d| serde_json::from_reader(d).ok())
    {
        Some(b) => b,
        None => {
            return Response::json(&serde_json::json!({"error": "invalid JSON body"}))
                .with_status_code(400);
        }
    };

    let tree_id = uuid::Uuid::new_v4().to_string();

    let repo_path = body.repo_path.as_ref().map(|p| {
        let path = std::path::Path::new(p);
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    });

    let now = chrono::Utc::now().timestamp();
    let model = body
        .model
        .clone()
        .unwrap_or_else(|| "qwen2.5-coder-7b-instruct".to_string());

    let meta = TreeMeta {
        id: tree_id.clone(),
        parent_id: None,
        repo_path,
        title: body.title,
        created_at: now,
        updated_at: now,
        leaf_id: None,
    };

    if let Err(e) = store.create_tree_file(&tree_id, &model) {
        return Response::json(&serde_json::json!({
            "error": format!("failed to create tree file: {}", e)
        }))
        .with_status_code(500);
    }

    let session_start_id = generate_entry_id();
    let session_start = Entry::SessionStart {
        id: session_start_id.clone(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = store.append_entry(&tree_id, &session_start) {
        return Response::json(&serde_json::json!({
            "error": format!("failed to write session_start: {}", e)
        }))
        .with_status_code(500);
    }

    let mut meta = meta;
    meta.leaf_id = Some(session_start_id.clone());

    if body.model.is_some() {
        let model_set = Entry::ModelSet {
            id: generate_entry_id(),
            parent_id: Some(session_start_id),
            timestamp: chrono::Utc::now().to_rfc3339(),
            model: model.clone(),
        };
        if let Err(e) = store.append_entry(&tree_id, &model_set) {
            return Response::json(&serde_json::json!({
                "error": format!("failed to write model_set: {}", e)
            }))
            .with_status_code(500);
        }
        meta.leaf_id = model_set.id().to_string().into();
        let _ = store.update_header(&tree_id, &serde_json::json!({"current_model": model}));
    }

    if let Err(e) = store.save_tree_meta(&meta) {
        return Response::json(&serde_json::json!({
            "error": format!("failed to save tree metadata: {}", e)
        }))
        .with_status_code(500);
    }

    Response::json(&meta).with_status_code(201)
}

/// GET /trees/{id} — get tree metadata
fn handle_get_tree(id: &str, store: &Store) -> Response {
    match store.get_tree(id) {
        Ok(Some(meta)) => Response::json(&meta),
        Ok(None) => {
            Response::json(&serde_json::json!({"error": format!("tree {} not found", id)}))
                .with_status_code(404)
        }
        Err(e) => Response::json(&serde_json::json!({"error": format!("{}", e)}))
            .with_status_code(500),
    }
}

/// PATCH /trees/{id} — update tree metadata
fn handle_update_tree(id: &str, request: &Request, store: &Store) -> Response {
    let body: UpdateTreeBody = match request
        .data()
        .and_then(|d| serde_json::from_reader(d).ok())
    {
        Some(b) => b,
        None => {
            return Response::json(&serde_json::json!({"error": "invalid JSON body"}))
                .with_status_code(400);
        }
    };

    let mut meta = match store.get_tree(id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return Response::json(&serde_json::json!({"error": format!("tree {} not found", id)}))
                .with_status_code(404);
        }
        Err(e) => {
            return Response::json(&serde_json::json!({"error": format!("{}", e)}))
                .with_status_code(500);
        }
    };

    if let Some(title) = body.title {
        meta.title = Some(title);
    }
    meta.updated_at = chrono::Utc::now().timestamp();

    if let Err(e) = store.save_tree_meta(&meta) {
        return Response::json(&serde_json::json!({
            "error": format!("failed to update tree: {}", e)
        }))
        .with_status_code(500);
    }

    Response::json(&meta)
}

/// GET /trees/{id}/entries — list all entries for a tree
fn handle_list_entries(id: &str, store: &Store) -> Response {
    match store.get_tree(id) {
        Ok(None) => {
            return Response::json(&serde_json::json!({"error": format!("tree {} not found", id)}))
                .with_status_code(404);
        }
        Err(e) => {
            return Response::json(&serde_json::json!({"error": format!("{}", e)}))
                .with_status_code(500);
        }
        Ok(Some(_)) => {}
    }

    match store.read_all_entries(id) {
        Ok(entries) => Response::json(&entries),
        Err(e) => Response::json(&serde_json::json!({"error": format!("{}", e)}))
            .with_status_code(500),
    }
}

/// POST /trees/{id}/message — send a user message to an active agent
fn handle_send_message(id: &str, request: &Request) -> Response {
    let body: SendMessageBody = match request
        .data()
        .and_then(|d| serde_json::from_reader(d).ok())
    {
        Some(b) => b,
        None => {
            return Response::json(&serde_json::json!({"error": "invalid JSON body"}))
                .with_status_code(400);
        }
    };

    if body.text.trim().is_empty() {
        return Response::json(&serde_json::json!({"error": "message text cannot be empty"}))
            .with_status_code(400);
    }

    match lifecycle::send_message(id, &body.text) {
        Ok(()) => Response::json(&serde_json::json!({"status": "sent"})),
        Err(e) => {
            Response::json(&serde_json::json!({"error": e})).with_status_code(409)
        }
    }
}

/// POST /trees/{id}/stop — stop an active agent
fn handle_stop_agent(id: &str) -> Response {
    match lifecycle::stop(id) {
        Ok(()) => Response::json(&serde_json::json!({"status": "stopping"})),
        Err(e) => Response::json(&serde_json::json!({"error": e})).with_status_code(404),
    }
}

/// GET /trees/{id}/stream — SSE event stream for an active agent
fn handle_sse_stream(id: &str) -> Response {
    let handle = match lifecycle::get_handle(id) {
        Some(h) => h,
        None => {
            return Response::json(&serde_json::json!({
                "error": format!("No active agent for tree {}", id)
            }))
            .with_status_code(404)
        }
    };

    let catch_up: Vec<ServerEvent> = {
        let buf = handle.event_buffer.lock().unwrap();
        buf.iter().cloned().collect()
    };

    let (tx, rx) = std::sync::mpsc::channel();
    handle.event_broadcast.lock().unwrap().push(tx);

    let stream = SseReconnectStream::new(catch_up, rx);

    Response {
            status_code: 200,
            headers: vec![
                (Cow::Borrowed("Content-Type"), Cow::Borrowed("text/event-stream")),
                (Cow::Borrowed("Cache-Control"), Cow::Borrowed("no-cache")),
            ],
            data: ResponseBody::from_reader(stream),
            upgrade: None,
        }
}

// ── Helpers ──

/// Generate a random 8-character hex entry ID.
fn generate_entry_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", nanos.wrapping_mul(2654435761))
}

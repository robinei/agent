use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;

use rouille::{router, Request, Response, ResponseBody};
use serde::Deserialize;

use agent_core::agent;
use agent_core::config::Config;
use agent_core::provider::Provider;
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

pub fn handle_request(request: &Request, store: &Arc<Store>, config: &Config) -> Response {
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
            handle_send_message(&id, request, store, config)
        },

        (POST) (/trees/{id: String}/auto-title) => {
            handle_auto_title(&id, store, config)
        },

        (POST) (/trees/{id: String}/stop) => {
            handle_stop_agent(&id)
        },

        (GET) (/trees/{id: String}/stream) => {
            handle_sse_stream(&id, store, config)
        },

        _ => {
            Response::json(&serde_json::json!({"error": "not found"}))
                .with_status_code(404)
        }
    )
}

// ── SSE Upgrade (bypasses BufWriter in tiny_http) ──

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
        sandbox: agent_core::types::TreeSandbox::default(),
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
///
/// Auto-spawns an agent for the tree if one isn't already running.
fn handle_send_message(
    id: &str,
    request: &Request,
    store: &Arc<Store>,
    config: &Config,
) -> Response {
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

    // Verify the tree exists
    match store.get_tree(id) {
        Ok(None) => {
            return Response::json(&serde_json::json!({
                "error": format!("tree {} not found", id)
            }))
            .with_status_code(404);
        }
        Err(e) => {
            return Response::json(&serde_json::json!({
                "error": format!("failed to read tree: {}", e)
            }))
            .with_status_code(500);
        }
        Ok(Some(_)) => {}
    }

    // Try to send message; if no active agent, spawn one first
    let result = lifecycle::send_message(id, &body.text);
    match result {
        Ok(()) => Response::json(&serde_json::json!({"status": "sent"})),
        Err(ref e) if e.contains("No active agent") => {
            // Auto-spawn agent for this tree
            log::info!("Auto-spawning agent for tree {}", id);
            if let Err(spawn_err) = lifecycle::spawn(id, (*store).clone(), config) {
                return Response::json(&serde_json::json!({
                    "error": format!("failed to spawn agent: {}", spawn_err)
                }))
                .with_status_code(500);
            }

            // Retry sending the message after spawn
            match lifecycle::send_message(id, &body.text) {
                Ok(()) => Response::json(&serde_json::json!({"status": "sent"})),
                Err(e) => Response::json(&serde_json::json!({
                    "error": format!("failed to send message after spawn: {}", e)
                }))
                .with_status_code(500),
            }
        }
        Err(e) => Response::json(&serde_json::json!({"error": e})).with_status_code(409),
    }
}

/// POST /trees/{id}/stop — stop an active agent
fn handle_stop_agent(id: &str) -> Response {
    match lifecycle::stop(id) {
        Ok(()) => Response::json(&serde_json::json!({"status": "stopping"})),
        Err(e) => Response::json(&serde_json::json!({"error": e})).with_status_code(404),
    }
}

/// POST /trees/{id}/auto-title — ask LLM to generate a title
fn handle_auto_title(id: &str, _store: &Store, config: &Config) -> Response {
    let provider = Provider::new(
        config.summary.base_url.clone(),
        config.summary.api_key.clone(),
        config.summary.model.clone(),
    );
    match agent::auto_title(_store, &provider, id) {
        Ok(title) => Response::json(&serde_json::json!({"title": title})),
        Err(e) => Response::json(&serde_json::json!({"error": e})).with_status_code(500),
    }
}

// ── SSE Upgrade (bypasses BufWriter in tiny_http) ──


struct SseUpgrade {
    handle: lifecycle::AgentHandle,
    tree_id: String,
    headers_written: bool,
}

impl rouille::Upgrade for SseUpgrade {
    fn build(&mut self, socket: Box<dyn rouille::ReadWrite + Send>) {
        log::info!("[sse-upgrade] Starting SSE write loop for tree {}", self.tree_id);

        let mut writer = std::io::BufWriter::with_capacity(8192, socket);

        // Headers are already sent by tiny_http as part of the upgrade response.
        // Only write headers if the initial response didn't carry them.
        if !self.headers_written {
            let _ = write!(
                &mut writer,
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: keep-alive\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Transfer-Encoding: chunked\r\n\
                 \r\n"
            );
            let _ = writer.flush();
        }

        // Send catch-up events from the ring buffer
        {
            let buf = self.handle.event_buffer.lock().unwrap();
            for event in buf.iter() {
                if let Ok(json) = serde_json::to_string(event) {
                    let _ = write!(&mut writer, "data: {}\n\n", json);
                }
            }
        }
        let _ = writer.flush();

        // Subscribe to broadcast channel and forward events until EOF
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut subs = self.handle.event_broadcast.lock().unwrap();
            subs.push(tx);
        }

        // Forward events from broadcast
        for event in &rx {
            if let Ok(json) = serde_json::to_string(&event) {
                let _ = write!(&mut writer, "data: {}\n\n", json);
                let _ = writer.flush();
            }
        }

        log::info!("[sse-upgrade] SSE stream ended for tree {}", self.tree_id);
        let _ = writer.flush();
    }
}

fn handle_sse_stream(id: &str, store: &Arc<Store>, config: &Config) -> Response {
    log::info!("[sse] Opening SSE stream for tree {}", id);

    // Auto-spawn agent if not already running
    if lifecycle::get_handle(id).is_none() {
        log::info!("[sse] Auto-spawning agent for tree {}", id);
        if let Err(e) = lifecycle::spawn(id, (*store).clone(), config) {
            return Response::json(&serde_json::json!({
                "error": format!("Failed to spawn agent: {}", e)
            }))
            .with_status_code(500);
        }
    }

    let handle = match lifecycle::get_handle(id) {
        Some(h) => h,
        None => {
            return Response::json(&serde_json::json!({
                "error": format!("No active agent for tree {}", id)
            }))
            .with_status_code(404)
        }
    };

    log::info!("[sse] Using SSE upgrade for tree {}", id);

    // SSE via Upgrade: takes over the raw socket, bypasses the BufWriter
    // in tiny_http, ensuring real-time streaming.
    // The upgrade response carries our SSE headers so the client sees only one
    // HTTP response (not the double-response that comes with a separate write).
    Response {
        status_code: 200,
        headers: vec![
            (Cow::Borrowed("Content-Type"), Cow::Borrowed("text/event-stream")),
            (Cow::Borrowed("Cache-Control"), Cow::Borrowed("no-cache")),
            (Cow::Borrowed("Connection"), Cow::Borrowed("keep-alive")),
            (Cow::Borrowed("Access-Control-Allow-Origin"), Cow::Borrowed("*")),
        ],
        data: ResponseBody::empty(),
        upgrade: Some(Box::new(SseUpgrade {
            handle,
            tree_id: id.to_string(),
            headers_written: true,
        })),
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
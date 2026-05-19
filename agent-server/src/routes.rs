use std::sync::Arc;

use agent_core::config::Config;
use agent_core::store::Store;
use serde::Deserialize;

use agent_core::agent;
use agent_core::provider::Provider;
use agent_core::types::{Entry, TreeMeta};

use crate::lifecycle;

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

pub fn dispatch(
    method: &str,
    path: &str,
    body: &[u8],
    store: &Arc<Store>,
    cfg: &Arc<Config>,
) -> (u16, Vec<u8>, &'static str) {
    if method == "GET" && path == "/" {
        return json(
            200,
            &serde_json::json!({"service": "agent-server", "version": "0.1.0"}),
        );
    }
    if method == "GET" && path == "/trees" {
        return handle_list_trees(store);
    }
    if method == "POST" && path == "/trees" {
        return handle_create_tree(body, store);
    }
    if let Some(rest) = path.strip_prefix("/trees/") {
        let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        return match (method, suffix) {
            ("GET", "") => handle_get_tree(id, store),
            ("PATCH", "") => handle_update_tree(id, body, store),
            ("GET", "entries") => handle_list_entries(id, store),
            ("POST", "message") => handle_send_message(id, body, store, cfg),
            ("POST", "stop") => handle_stop_agent(id),
            ("POST", "auto-title") => handle_auto_title(id, store, cfg),
            _ => not_found(),
        };
    }
    not_found()
}

fn handle_list_trees(store: &Store) -> (u16, Vec<u8>, &'static str) {
    match store.list_trees() {
        Ok(trees) => json(200, &trees),
        Err(e) => {
            json(500, &serde_json::json!({"error": format!("{}", e)}))
        }
    }
}

fn handle_create_tree(body: &[u8], store: &Store) -> (u16, Vec<u8>, &'static str) {
    let body: CreateTreeBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => {
            return json(400, &serde_json::json!({"error": "invalid JSON body"}));
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
        return json(
            500,
            &serde_json::json!({"error": format!("failed to create tree file: {}", e)}),
        );
    }

    let session_start_id = agent_core::util::generate_entry_id();
    let session_start = Entry::SessionStart {
        id: session_start_id.clone(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = store.append_entry(&tree_id, &session_start) {
        return json(
            500,
            &serde_json::json!({"error": format!("failed to write session_start: {}", e)}),
        );
    }

    let mut meta = meta;
    meta.leaf_id = Some(session_start_id.clone());

    if body.model.is_some() {
        let model_set = Entry::ModelSet {
            id: agent_core::util::generate_entry_id(),
            parent_id: Some(session_start_id),
            timestamp: chrono::Utc::now().to_rfc3339(),
            model: model.clone(),
        };
        if let Err(e) = store.append_entry(&tree_id, &model_set) {
            return json(
                500,
                &serde_json::json!({"error": format!("failed to write model_set: {}", e)}),
            );
        }
        meta.leaf_id = model_set.id().to_string().into();
        let _ = store.update_header(&tree_id, &serde_json::json!({"current_model": model}));
    }

    if let Err(e) = store.save_tree_meta(&meta) {
        return json(
            500,
            &serde_json::json!({"error": format!("failed to save tree metadata: {}", e)}),
        );
    }

    json(201, &meta)
}

fn handle_get_tree(id: &str, store: &Store) -> (u16, Vec<u8>, &'static str) {
    match store.get_tree(id) {
        Ok(Some(meta)) => json(200, &meta),
        Ok(None) => json(404, &serde_json::json!({"error": format!("tree {} not found", id)})),
        Err(e) => json(500, &serde_json::json!({"error": format!("{}", e)})),
    }
}

fn handle_update_tree(id: &str, body: &[u8], store: &Store) -> (u16, Vec<u8>, &'static str) {
    let body: UpdateTreeBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => {
            return json(400, &serde_json::json!({"error": "invalid JSON body"}));
        }
    };

    let mut meta = match store.get_tree(id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return json(404, &serde_json::json!({"error": format!("tree {} not found", id)}));
        }
        Err(e) => {
            return json(500, &serde_json::json!({"error": format!("{}", e)}));
        }
    };

    if let Some(title) = body.title {
        meta.title = Some(title);
    }
    meta.updated_at = chrono::Utc::now().timestamp();

    if let Err(e) = store.save_tree_meta(&meta) {
        return json(
            500,
            &serde_json::json!({"error": format!("failed to update tree: {}", e)}),
        );
    }

    json(200, &meta)
}

fn handle_list_entries(id: &str, store: &Store) -> (u16, Vec<u8>, &'static str) {
    match store.get_tree(id) {
        Ok(None) => {
            return json(404, &serde_json::json!({"error": format!("tree {} not found", id)}));
        }
        Err(e) => {
            return json(500, &serde_json::json!({"error": format!("{}", e)}));
        }
        Ok(Some(_)) => {}
    }

    match store.read_all_entries(id) {
        Ok(entries) => json(200, &entries),
        Err(e) => json(500, &serde_json::json!({"error": format!("{}", e)})),
    }
}

fn handle_send_message(
    id: &str,
    body: &[u8],
    store: &Arc<Store>,
    cfg: &Arc<Config>,
) -> (u16, Vec<u8>, &'static str) {
    let body: SendMessageBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => {
            return json(400, &serde_json::json!({"error": "invalid JSON body"}));
        }
    };

    if body.text.trim().is_empty() {
        return json(
            400,
            &serde_json::json!({"error": "message text cannot be empty"}),
        );
    }

    match store.get_tree(id) {
        Ok(None) => {
            return json(404, &serde_json::json!({"error": format!("tree {} not found", id)}));
        }
        Err(e) => {
            return json(
                500,
                &serde_json::json!({"error": format!("failed to read tree: {}", e)}),
            );
        }
        Ok(Some(_)) => {}
    }

    let result = lifecycle::send_message(id, &body.text);
    match result {
        Ok(()) => json(200, &serde_json::json!({"status": "sent"})),
        Err(ref e) if e.contains("No active agent") => {
            log::info!("Auto-spawning agent for tree {}", id);
            if let Err(spawn_err) = lifecycle::spawn(id, store.clone(), cfg) {
                return json(
                    500,
                    &serde_json::json!({
                        "error": format!("failed to spawn agent: {}", spawn_err)
                    }),
                );
            }
            match lifecycle::send_message(id, &body.text) {
                Ok(()) => json(200, &serde_json::json!({"status": "sent"})),
                Err(e) => json(
                    500,
                    &serde_json::json!({"error": format!("failed to send message after spawn: {}", e)}),
                ),
            }
        }
        Err(e) => json(409, &serde_json::json!({"error": e})),
    }
}

fn handle_stop_agent(id: &str) -> (u16, Vec<u8>, &'static str) {
    match lifecycle::stop(id) {
        Ok(()) => json(200, &serde_json::json!({"status": "stopping"})),
        Err(e) => json(404, &serde_json::json!({"error": e})),
    }
}

fn handle_auto_title(id: &str, _store: &Store, config: &Config) -> (u16, Vec<u8>, &'static str) {
    let provider = Provider::new(
        config.summary.base_url.clone(),
        config.summary.api_key.clone(),
        config.summary.model.clone(),
    );
    match agent::auto_title(_store, &provider, id) {
        Ok(title) => json(200, &serde_json::json!({"title": title})),
        Err(e) => json(500, &serde_json::json!({"error": e})),
    }
}

fn json<T: serde::Serialize>(status: u16, v: &T) -> (u16, Vec<u8>, &'static str) {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    (status, body, "application/json")
}

fn not_found() -> (u16, Vec<u8>, &'static str) {
    json(404, &serde_json::json!({"error": "not found"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_dispatch_static() {
        let (status, body, ct) = dispatch("GET", "/", &[], &Arc::new(Store::default()), &Arc::new(Config::default()));
        assert_eq!(status, 200);
        assert_eq!(ct, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["service"], "agent-server");
    }

    #[test]
    fn test_dispatch_not_found() {
        let (status, body, ct) = dispatch("GET", "/nonexistent", &[], &Arc::new(Store::default()), &Arc::new(Config::default()));
        assert_eq!(status, 404);
        assert_eq!(ct, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "not found");
    }

    #[test]
    fn test_dispatch_create_tree_bad_body() {
        let (status, _, _) = dispatch("POST", "/trees", b"not json", &Arc::new(Store::default()), &Arc::new(Config::default()));
        assert_eq!(status, 400);
    }

    #[test]
    fn test_dispatch_tree_entries_not_found() {
        let (status, _, _) = dispatch("GET", "/trees/no-such-tree/entries", &[], &Arc::new(Store::default()), &Arc::new(Config::default()));
        assert_eq!(status, 404);
    }

    #[test]
    fn test_json_helper() {
        let (status, body, ct) = json(201, &serde_json::json!({"key": "val"}));
        assert_eq!(status, 201);
        assert_eq!(ct, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["key"], "val");
    }

    #[test]
    fn test_dispatch_get_tree_no_such() {
        let store = Arc::new(Store::default());
        let (status, _body, _) = dispatch("GET", "/trees/nonexistent", &[], &store, &Arc::new(Config::default()));
        assert_eq!(status, 404);
    }

    #[test]
    fn test_dispatch_create_and_get_tree() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::new(tmp.path().join(".agent")));
        let cfg = Arc::new(Config::default());

        let create_body = serde_json::json!({"title": "test-tree"});
        let (status, body, _) = dispatch("POST", "/trees", &serde_json::to_vec(&create_body).unwrap(), &store, &cfg);
        assert_eq!(status, 201);
        let meta: TreeMeta = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta.title.unwrap(), "test-tree");

        let get_path = format!("/trees/{}", meta.id);
        let (status, body, _) = dispatch("GET", &get_path, &[], &store, &cfg);
        assert_eq!(status, 200);
        let fetched: TreeMeta = serde_json::from_slice(&body).unwrap();
        assert_eq!(fetched.id, meta.id);

        let entries_path = format!("/trees/{}/entries", meta.id);
        let (status, body, _) = dispatch("GET", &entries_path, &[], &store, &cfg);
        assert_eq!(status, 200);
        let entries: Vec<Entry> = serde_json::from_slice(&body).unwrap();
        assert!(entries.iter().any(|e| matches!(e, Entry::SessionStart { .. })));
    }
}

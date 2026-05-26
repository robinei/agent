use std::sync::Arc;

use serde::Deserialize;

use crate::spawner;
use agent_core::config::Config;
use agent_core::types::{TreeMeta, TreeSandbox};

#[derive(Deserialize)]
pub struct CreateTreeBody {
    pub title: Option<String>,
    pub repo_path: Option<String>,
    pub sandbox: Option<TreeSandbox>,
}

#[derive(Deserialize)]
pub struct UpdateTreeBody {
    pub title: Option<String>,
    pub sandbox: Option<TreeSandbox>,
}

pub fn dispatch(
    method: &str,
    path: &str,
    body: &[u8],
    cfg: &Arc<Config>,
) -> (u16, Vec<u8>, &'static str) {
    if method == "GET" && path == "/" {
        return json(
            200,
            &serde_json::json!({"service": "agent-server", "version": "0.1.0"}),
        );
    }
    if method == "GET" && path == "/trees" {
        return handle_list_trees();
    }
    if method == "POST" && path == "/trees" {
        return handle_create_tree(body, cfg);
    }
    if let Some(rest) = path.strip_prefix("/trees/") {
        let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        return match (method, suffix) {
            ("GET", "") => handle_get_tree(id),
            ("PATCH", "") => handle_update_tree(id, body),
            ("POST", "stop") => handle_stop_agent(id),
            _ => not_found(),
        };
    }
    not_found()
}

fn agent_dir() -> std::path::PathBuf {
    agent_core::config::agent_dir()
}

fn handle_list_trees() -> (u16, Vec<u8>, &'static str) {
    match agent_core::tree_io::list_trees(&agent_dir()) {
        Ok(trees) => json(200, &trees),
        Err(e) => {
            json(500, &serde_json::json!({"error": e.to_string()}))
        }
    }
}

fn handle_create_tree(body: &[u8], cfg: &Config) -> (u16, Vec<u8>, &'static str) {
    let body: CreateTreeBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => {
            return json(400, &serde_json::json!({"error": "invalid JSON body"}));
        }
    };

    let tree_id = uuid::Uuid::new_v4().to_string();

    let sandbox = body.sandbox.unwrap_or_default();

    let repo_path = match &body.repo_path {
        Some(p) => {
            let path = std::path::Path::new(p);
            match agent_core::types::validate_repo_path(path, &cfg.sandbox.defaults.hide, &sandbox) {
                Ok(canon) => Some(canon),
                Err(e) => return json(400, &serde_json::json!({"error": e.to_string()})),
            }
        }
        None => None,
    };

    let now = chrono::Utc::now().timestamp();

    let meta = TreeMeta {
        id: tree_id.clone(),
        parent_id: None,
        repo_path,
        title: body.title,
        created_at: now,
        updated_at: now,
        leaf_id: None,
        sandbox,
    };

    if let Err(e) = agent_core::tree_io::create_tree(&agent_dir(), &meta) {
        return json(
            500,
            &serde_json::json!({"error": format!("failed to create tree: {}", e)}),
        );
    }

    json(201, &meta)
}

fn handle_get_tree(id: &str) -> (u16, Vec<u8>, &'static str) {
    match agent_core::tree_io::read_meta(&agent_dir(), id) {
        Ok(Some(meta)) => json(200, &meta),
        Ok(None) => json(404, &serde_json::json!({"error": format!("tree {} not found", id)})),
        Err(e) => json(500, &serde_json::json!({"error": e.to_string()})),
    }
}

fn handle_update_tree(id: &str, body: &[u8]) -> (u16, Vec<u8>, &'static str) {
    let body: UpdateTreeBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => {
            return json(400, &serde_json::json!({"error": "invalid JSON body"}));
        }
    };

    let mut meta = match agent_core::tree_io::read_meta(&agent_dir(), id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return json(404, &serde_json::json!({"error": format!("tree {} not found", id)}));
        }
        Err(e) => {
            return json(500, &serde_json::json!({"error": e.to_string()}));
        }
    };

    if let Some(title) = body.title {
        meta.title = Some(title);
    }
    if let Some(sandbox) = body.sandbox {
        meta.sandbox = sandbox;
    }
    meta.updated_at = chrono::Utc::now().timestamp();

    if let Err(e) = agent_core::tree_io::write_meta(&agent_dir(), &meta) {
        return json(
            500,
            &serde_json::json!({"error": format!("failed to update tree: {}", e)}),
        );
    }

    json(200, &meta)
}

fn handle_stop_agent(id: &str) -> (u16, Vec<u8>, &'static str) {
    match spawner::worker_stop(id) {
        Ok(()) => json(200, &serde_json::json!({"status": "stopping"})),
        Err(e) => json(404, &serde_json::json!({"error": e.to_string()})),
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
    use std::sync::Mutex;
    use tempfile::TempDir;

    static AGENT_DIR_LOCK: Mutex<()> = Mutex::new(());

    /// Run a test with AGENT_DIR set to a temp dir. Keeps TempDir alive,
    /// recovers from poisoned mutex, and restores the original env var.
    fn with_agent_dir<F>(f: F)
    where
        F: FnOnce(Arc<Config>, &TempDir),
    {
        let _guard = AGENT_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let old = std::env::var_os("AGENT_DIR");
        std::env::set_var("AGENT_DIR", tmp.path());
        let cfg = Arc::new(Config::default());
        f(cfg, &tmp);
        match old {
            Some(v) => std::env::set_var("AGENT_DIR", v),
            None => std::env::remove_var("AGENT_DIR"),
        }
    }

    #[test]
    fn test_dispatch_static() {
        let (status, body, ct) = dispatch("GET", "/", &[], &Arc::new(Config::default()));
        assert_eq!(status, 200);
        assert_eq!(ct, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["service"], "agent-server");
    }

    #[test]
    fn test_dispatch_not_found() {
        let (status, body, ct) = dispatch("GET", "/nonexistent", &[], &Arc::new(Config::default()));
        assert_eq!(status, 404);
        assert_eq!(ct, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "not found");
    }

    #[test]
    fn test_dispatch_create_tree_bad_body() {
        let (status, _, _) = dispatch("POST", "/trees", b"not json", &Arc::new(Config::default()));
        assert_eq!(status, 400);
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
        let (status, _body, _) = dispatch("GET", "/trees/nonexistent", &[], &Arc::new(Config::default()));
        assert_eq!(status, 404);
    }

    #[test]
    fn test_dispatch_create_and_get_tree() {
        with_agent_dir(|cfg, _tmp| {
            let create_body = serde_json::json!({"title": "test-tree"});
            let (status, body, _) = dispatch("POST", "/trees", &serde_json::to_vec(&create_body).unwrap(), &cfg);
            assert_eq!(status, 201);
            let meta: TreeMeta = serde_json::from_slice(&body).unwrap();
            assert_eq!(meta.title.unwrap(), "test-tree");

            let get_path = format!("/trees/{}", meta.id);
            let (status, body, _) = dispatch("GET", &get_path, &[], &cfg);
            assert_eq!(status, 200);
            let fetched: TreeMeta = serde_json::from_slice(&body).unwrap();
            assert_eq!(fetched.id, meta.id);
        });
    }

    #[test]
    fn test_create_tree_rejects_repo_inside_default_hide() {
        use agent_core::config::{SandboxConfig, SandboxDefaults};

        with_agent_dir(|_cfg, tmp| {
            // Create a hidden directory inside the temp dir
            let hidden_dir = tmp.path().join(".ssh");
            std::fs::create_dir_all(&hidden_dir).unwrap();

            let cfg = Arc::new(Config {
                sandbox: SandboxConfig {
                    enabled: false,
                    bwrap_path: None,
                    defaults: SandboxDefaults {
                        hide: vec![hidden_dir.clone()],
                    },
                },
                ..Config::default()
            });

            let create_body = serde_json::json!({
                "title": "test-tree",
                "repo_path": hidden_dir.to_str().unwrap(),
            });
            let (status, body, _) = dispatch("POST", "/trees", &serde_json::to_vec(&create_body).unwrap(), &cfg);
            assert_eq!(status, 400);
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let err = v["error"].as_str().unwrap();
            assert!(err.contains("hidden"), "error should mention 'hidden': {}", err);
        });
    }
}
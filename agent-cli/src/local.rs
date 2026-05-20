use std::sync::Arc;

use agent_core::config::Config;
use agent_core::store::Store;
use agent_core::types::{Entry, TreeMeta, TreeSandbox};

/// Handles tree CRUD and lifecycle operations by calling Store and lifecycle
/// directly, bypassing HTTP entirely. Used by the embedded server path.
pub struct LocalClient {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
}

impl LocalClient {
    pub fn new(store: Arc<Store>, config: Arc<Config>) -> Self {
        Self { store, config }
    }

    pub fn list_trees(&self) -> Result<Vec<TreeMeta>, String> {
        self.store
            .list_trees()
            .map_err(|e| format!("{}", e))
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
    ) -> Result<TreeMeta, String> {
        let tree_id = uuid::Uuid::new_v4().to_string();

        let sandbox = TreeSandbox {
            writable: writable.to_vec(),
            network,
            hide: hide.to_vec(),
            unhide: unhide.to_vec(),
        };

        let repo_path = match repo_path {
            Some(p) => {
                let path = std::path::Path::new(p);
                match agent_core::types::validate_repo_path(
                    path,
                    &self.config.sandbox.defaults.hide,
                    &sandbox,
                ) {
                    Ok(canon) => Some(canon),
                    Err(e) => return Err(e),
                }
            }
            None => None,
        };

        let now = chrono::Utc::now().timestamp();
        let model = model
            .unwrap_or("qwen2.5-coder-7b-instruct")
            .to_string();

        let meta = TreeMeta {
            id: tree_id.clone(),
            parent_id: None,
            repo_path,
            title: title.map(|s| s.to_string()),
            created_at: now,
            updated_at: now,
            leaf_id: None,
            sandbox,
        };

        self.store
            .create_tree_file(&tree_id, &model)
            .map_err(|e| format!("failed to create tree file: {}", e))?;

        let session_start_id = agent_core::util::generate_entry_id();
        let session_start = Entry::SessionStart {
            id: session_start_id.clone(),
            parent_id: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        self.store
            .append_entry(&tree_id, &session_start)
            .map_err(|e| format!("failed to write session_start: {}", e))?;

let mut meta = meta;
        meta.leaf_id = Some(session_start_id.clone());

        // Write ModelSet only if a non-default model was specified (matches routes.rs behavior)
        if model != "qwen2.5-coder-7b-instruct" {
            let model_set = Entry::ModelSet {
                id: agent_core::util::generate_entry_id(),
                parent_id: Some(session_start_id),
                timestamp: chrono::Utc::now().to_rfc3339(),
                model: model.clone(),
            };
            self.store
                .append_entry(&tree_id, &model_set)
                .map_err(|e| format!("failed to write model_set: {}", e))?;
            meta.leaf_id = model_set.id().to_string().into();
            let _ = self
                .store
                .update_header(&tree_id, &serde_json::json!({"current_model": model}));
        }

        self.store
            .save_tree_meta(&meta)
            .map_err(|e| format!("failed to save tree metadata: {}", e))?;

        Ok(meta)
    }

    pub fn get_tree(&self, id: &str) -> Result<TreeMeta, String> {
        self.store
            .get_tree(id)
            .map_err(|e| format!("{}", e))?
            .ok_or_else(|| format!("tree {} not found", id))
    }

    pub fn get_entries(&self, tree_id: &str) -> Result<Vec<Entry>, String> {
        self.store
            .read_all_entries(tree_id)
            .map_err(|e| format!("{}", e))
    }

    pub fn stop_agent(&self, tree_id: &str) -> Result<(), String> {
        agent_server::lifecycle::worker_stop(tree_id)
    }

    pub fn auto_title(&self, tree_id: &str) -> Result<String, String> {
        let provider = agent_core::provider::Provider::new(
            self.config.summary.base_url.clone(),
            self.config.summary.api_key.clone(),
            self.config.summary.model.clone(),
            false,
        );
        agent_core::agent::auto_title(&self.store, &provider, tree_id)
    }
}
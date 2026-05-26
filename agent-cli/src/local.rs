use std::sync::Arc;

use agent_core::config::Config;
use agent_core::tree_io::TreeIoError;
use agent_core::types::ValidateRepoError;
use agent_core::types::{TreeMeta, TreeSandbox};

#[derive(Debug, thiserror::Error)]
pub enum LocalClientError {
    #[error(transparent)]
    Io(#[from] TreeIoError),
    #[error(transparent)]
    Validation(#[from] ValidateRepoError),
    #[error("{0}")]
    Other(String),
}

impl From<String> for LocalClientError {
    fn from(s: String) -> Self {
        LocalClientError::Other(s)
    }
}

/// Handles tree CRUD and lifecycle operations by calling tree_io and spawner
/// directly, bypassing HTTP entirely. Used by the embedded server path.
pub struct LocalClient {
    pub config: Arc<Config>,
}

impl LocalClient {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    fn agent_dir(&self) -> std::path::PathBuf {
        agent_core::config::agent_dir()
    }

    pub fn list_trees(&self) -> Result<Vec<TreeMeta>, LocalClientError> {
        Ok(agent_core::tree_io::list_trees(&self.agent_dir())?)
    }

    pub fn create_tree(
        &self,
        title: Option<&str>,
        repo_path: Option<&str>,
        _model: Option<&str>,
        writable: &[std::path::PathBuf],
        network: Option<bool>,
        hide: &[std::path::PathBuf],
        unhide: &[std::path::PathBuf],
    ) -> Result<TreeMeta, LocalClientError> {
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
                let canon = agent_core::types::validate_repo_path(
                    path,
                    &self.config.sandbox.defaults.hide,
                    &sandbox,
                )?;
                Some(canon)
            }
            None => None,
        };

        let now = chrono::Utc::now().timestamp();

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

        agent_core::tree_io::create_tree(&self.agent_dir(), &meta)?;

        Ok(meta)
    }

    pub fn get_tree(&self, id: &str) -> Result<TreeMeta, LocalClientError> {
        agent_core::tree_io::read_meta(&self.agent_dir(), id)?
            .ok_or_else(|| LocalClientError::Other(format!("tree {} not found", id)))
    }

    pub fn stop_agent(&self, tree_id: &str) -> Result<(), LocalClientError> {
        agent_server::spawner::worker_stop(tree_id)
            .map_err(|e| LocalClientError::Other(e.to_string()))
    }
}
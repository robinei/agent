use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use log::warn;

use agent_core::config::agent_dir;
use agent_core::types::{Entry, TreeHeader, TreeMeta};

// ── Error type ──

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Tree not found: {0}")]
    NotFound(String),
    #[error("Invalid header in tree file: {0}")]
    InvalidHeader(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

// ── Store struct ──

/// Manages tree data stored as JSONL files under a base directory.
///
/// Each worker owns exactly one tree, identified at construction.
/// Metadata is cached: `None` = uncached, `Some(None)` = confirmed absent, `Some(Some(_))` = loaded.
#[derive(Debug)]
pub struct Store {
    base_dir: PathBuf,
    tree_id: String,
    meta_cache: RefCell<Option<Option<TreeMeta>>>,
}

impl Store {
    pub fn new(base_dir: PathBuf, tree_id: &str) -> Self {
        Self {
            base_dir,
            tree_id: tree_id.to_string(),
            meta_cache: RefCell::new(None),
        }
    }

    /// Convenience constructor using the default agent directory.
    pub fn new_default(tree_id: &str) -> Self {
        Self::new(agent_dir(), tree_id)
    }

    pub fn base_dir(&self) -> &PathBuf {
        &self.base_dir
    }

    pub fn tree_id(&self) -> &str {
        &self.tree_id
    }

    fn jsonl_path(&self) -> PathBuf {
        self.base_dir
            .join("trees")
            .join(&self.tree_id)
            .join("data.jsonl")
    }

    // ── Tree metadata I/O ──

    fn load_tree_meta(&self) -> Result<Option<TreeMeta>> {
        agent_core::tree_io::read_meta(&self.base_dir, &self.tree_id)
            .map_err(|e| StoreError::Io(std::io::Error::other(e)))
    }

    pub fn save_tree_meta(&self, meta: &TreeMeta) -> Result<()> {
        agent_core::tree_io::write_meta(&self.base_dir, meta)
            .map_err(|e| StoreError::Io(std::io::Error::other(e)))?;
        *self.meta_cache.borrow_mut() = Some(Some(meta.clone()));
        Ok(())
    }

    pub fn get_tree(&self) -> Result<TreeMeta> {
        // Check cache first
        if let Some(Some(ref meta)) = *self.meta_cache.borrow() {
            return Ok(meta.clone());
        }
        if self.meta_cache.borrow().is_some() {
            // Cache holds `Some(None)` — confirmed absent on disk.
            return Err(StoreError::NotFound(self.tree_id.clone()));
        }
        // Cache miss: load from disk and cache the result.
        match self.load_tree_meta()? {
            Some(meta) => {
                *self.meta_cache.borrow_mut() = Some(Some(meta.clone()));
                Ok(meta)
            }
            None => {
                *self.meta_cache.borrow_mut() = Some(None);
                Err(StoreError::NotFound(self.tree_id.clone()))
            }
        }
    }

    // ── Tree file (JSONL) I/O ──

    pub fn create_tree_file(&self) -> Result<()> {
        let path = self.jsonl_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let header = TreeHeader {
            kind: "meta".to_string(),
            version: 1,
            id: self.tree_id.clone(),
        };
        let mut line = serde_json::to_string(&header)?;
        line.push('\n');
        std::fs::write(&path, line.as_bytes())?;
        Ok(())
    }

    pub fn append_entry(&self, entry: &Entry) -> Result<()> {
        let path = self.jsonl_path();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn read_all_entries(&self) -> Result<Vec<Entry>> {
        let path = self.jsonl_path();
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut lines = reader.lines();

        // Line 1: header (skip, but validate)
        match lines.next() {
            Some(Ok(line)) => {
                if let Ok(header) = serde_json::from_str::<TreeHeader>(&line) {
                    if header.kind != "meta" {
                        warn!(
                            "Tree {} header has unexpected kind: {}",
                            self.tree_id, header.kind
                        );
                    }
                }
            }
            Some(Err(e)) => return Err(StoreError::Io(e)),
            None => return Ok(Vec::new()),
        }

        // Remaining lines: entries
        let mut entries = Vec::new();
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    warn!("Skipping unparseable entry in {}: {}", self.tree_id, e);
                }
            }
        }

        Ok(entries)
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::types::*;
    use tempfile::TempDir;

    fn make_store(name: &str, tree_id: &str) -> (Store, TempDir) {
        let dir = TempDir::with_prefix(&format!("agent-store-{}", name)).unwrap();
        let store = Store::new(dir.path().to_path_buf(), tree_id);
        (store, dir)
    }

    #[test]
    fn test_create_tree_writes_subdir() {
        let tree_id = "subdir-001";
        let (store, dir) = make_store("subdir", tree_id);

        store.create_tree_file().unwrap();

        let meta = TreeMeta {
            id: tree_id.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        store.save_tree_meta(&meta).unwrap();

        let tree_dir = dir.path().join("trees").join(tree_id);
        assert!(
            tree_dir.join("data.jsonl").exists(),
            "data.jsonl should exist in tree subdir"
        );
        assert!(
            tree_dir.join("meta.json").exists(),
            "meta.json should exist in tree subdir"
        );
    }

    #[test]
    fn test_create_and_read_tree() {
        let (store, _dir) = make_store("create_read", "test-001");

        store.create_tree_file().unwrap();

        let entries = store.read_all_entries().unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_append_and_read_entries() {
        let (store, _dir) = make_store("append_read", "test-002");

        store.create_tree_file().unwrap();

        let entry = Entry::SessionStart {
            id: "00000001".into(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00Z".into(),
        };
        store.append_entry(&entry).unwrap();

        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let entry2 = Entry::Message {
            id: "00000002".into(),
            parent_id: Some("00000001".into()),
            timestamp: "2026-01-01T00:00:01Z".into(),
            message: msg,
        };
        store.append_entry(&entry2).unwrap();

        let entries = store.read_all_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id(), "00000001");
        assert_eq!(entries[1].id(), "00000002");
    }

    #[test]
    fn test_tree_meta_roundtrip() {
        let tree_id = "test-003";
        let (store, _dir) = make_store("meta_rt", tree_id);

        let meta = TreeMeta {
            id: tree_id.to_string(),
            parent_id: None,
            repo_path: None,
            title: Some("Test Tree".into()),
            created_at: 1000,
            updated_at: 1001,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };

        store.save_tree_meta(&meta).unwrap();

        let loaded = store.load_tree_meta().unwrap().unwrap();
        assert_eq!(loaded.id, tree_id);
        assert_eq!(loaded.title.unwrap(), "Test Tree");

        let cached = store.get_tree().unwrap();
        assert_eq!(cached.created_at, 1000);
    }

    #[test]
    fn test_header_is_immutable() {
        let tree_id = "test-004";
        let (store, _dir) = make_store("header_immutable", tree_id);

        store.create_tree_file().unwrap();

        // Header should contain only structural fields
        let content = std::fs::read_to_string(store.jsonl_path()).unwrap();
        let first_line = content.lines().next().unwrap();
        let header: TreeHeader = serde_json::from_str(first_line).unwrap();
        assert_eq!(header.kind, "meta");
        assert_eq!(header.version, 1);
        assert_eq!(header.id, tree_id);
    }
}

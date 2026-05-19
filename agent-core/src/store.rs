use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use log::warn;

use crate::config::agent_dir;
use crate::types::{Entry, TreeHeader, TreeId, TreeMeta};

// ── In-memory index cache ──

static INDEX_CACHE: LazyLock<Mutex<HashMap<TreeId, TreeMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-file mutex for serializing appends to the same .jsonl file.
static FILE_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_file_lock<F, T>(id: &str, f: F) -> T
where
    F: FnOnce() -> T,
{
    let mut locks = FILE_LOCKS.lock().unwrap();
    let lock = locks.entry(id.to_string()).or_default().clone();
    drop(locks);
    let _guard = lock.lock().unwrap();
    f()
}

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
/// Default base is `~/.agent/`. Override via `Store::new()` for testing.
#[derive(Clone)]
pub struct Store {
    base_dir: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            base_dir: agent_dir(),
        }
    }
}

impl Store {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn base_dir(&self) -> &PathBuf {
        &self.base_dir
    }

    fn tree_dir(&self) -> PathBuf {
        self.base_dir.join("trees")
    }

    /// Returns the per-tree directory path (used by bwrap arg construction).
    pub fn tree_dir_for(&self, id: &str) -> PathBuf {
        self.tree_dir().join(id)
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.tree_dir_for(id).join("meta.json")
    }

    pub fn jsonl_path(&self, id: &str) -> PathBuf {
        self.tree_dir_for(id).join("data.jsonl")
    }

    // ── Index cache helpers ──

    fn update_index_cache(&self, meta: &TreeMeta) {
        INDEX_CACHE.lock().unwrap().insert(meta.id.clone(), meta.clone());
    }

    // ── Tree metadata I/O ──

    pub fn load_tree_meta(&self, id: &str) -> Result<Option<TreeMeta>> {
        let path = self.meta_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&content)?))
    }

    pub fn save_tree_meta(&self, meta: &TreeMeta) -> Result<()> {
        let path = self.meta_path(&meta.id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic write: write to temp, rename over target
        let tmp = path.with_extension("meta.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(meta)?)?;
        std::fs::rename(&tmp, &path)?;
        self.update_index_cache(meta);
        Ok(())
    }

    pub fn get_tree(&self, id: &str) -> Result<Option<TreeMeta>> {
        // Check cache first
        if let Some(meta) = INDEX_CACHE.lock().unwrap().get(id).cloned() {
            return Ok(Some(meta));
        }
        // Fall back to disk
        self.load_tree_meta(id)
    }

    pub fn update_tree(&self, meta: &TreeMeta) -> Result<()> {
        self.save_tree_meta(meta)
    }

    pub fn list_trees(&self) -> Result<Vec<TreeMeta>> {
        let cache = INDEX_CACHE.lock().unwrap();
        let mut trees: Vec<TreeMeta> = cache.values().cloned().collect();
        trees.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(trees)
    }

    pub fn clear_cache(&self, id: &str) {
        INDEX_CACHE.lock().unwrap().remove(id);
    }

    pub fn rebuild_index(&self) -> Result<Vec<TreeMeta>> {
        let mut trees = Vec::new();
        let dir = self.tree_dir();
        if !dir.exists() {
            return Ok(trees);
        }
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let meta_path = path.join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            match std::fs::read_to_string(&meta_path) {
                Ok(content) => {
                    match serde_json::from_str::<TreeMeta>(&content) {
                        Ok(meta) => {
                            self.update_index_cache(&meta);
                            trees.push(meta);
                        }
                        Err(e) => {
                            warn!("Skipping corrupt meta file {:?}: {}", meta_path, e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Skipping unreadable meta file {:?}: {}", meta_path, e);
                }
            }
        }
        trees.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        // Write index.json as cache (best-effort)
        let index_path = self.base_dir.join("index.json");
        let _ = std::fs::write(&index_path, serde_json::to_string(&trees).unwrap_or_default());
        Ok(trees)
    }

    // ── Tree file (JSONL) I/O ──

    pub fn create_tree_file(&self, id: &str, model: &str) -> Result<()> {
        let path = self.jsonl_path(id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let header = TreeHeader {
            kind: "meta".to_string(),
            version: 1,
            id: id.to_string(),
            total_tokens: 0,
            current_model: model.to_string(),
        };
        let mut line = serde_json::to_string(&header)?;
        line.push('\n');
        std::fs::write(&path, line.as_bytes())?;
        Ok(())
    }

    pub fn append_entry(&self, tree_id: &str, entry: &Entry) -> Result<()> {
        let path = self.jsonl_path(tree_id);
        with_file_lock(tree_id, || -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            let mut line = serde_json::to_string(entry)?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })
    }

    pub fn read_all_entries(&self, tree_id: &str) -> Result<Vec<Entry>> {
        let path = self.jsonl_path(tree_id);
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut lines = reader.lines();

        // Line 1: header (skip, but validate)
        match lines.next() {
            Some(Ok(line)) => {
                if let Ok(header) = serde_json::from_str::<TreeHeader>(&line) {
                    if header.kind != "meta" {
                        warn!("Tree {} header has unexpected kind: {}", tree_id, header.kind);
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
                    warn!("Skipping unparseable entry in {}: {}", tree_id, e);
                }
            }
        }

        Ok(entries)
    }

    pub fn update_header(&self, tree_id: &str, updates: &serde_json::Value) -> Result<()> {
        let path = self.jsonl_path(tree_id);
        with_file_lock(tree_id, || -> Result<()> {
            let content = std::fs::read_to_string(&path)?;
            let first_newline = content.find('\n').unwrap_or(content.len());
            let header_line = &content[..first_newline];
            let mut header: TreeHeader = serde_json::from_str(header_line)?;

            if let Some(tokens) = updates.get("total_tokens").and_then(|v| v.as_u64()) {
                header.total_tokens = tokens;
            }
            if let Some(model) = updates.get("current_model").and_then(|v| v.as_str()) {
                header.current_model = model.to_string();
            }

            let rest = if first_newline < content.len() {
                &content[first_newline + 1..]
            } else {
                ""
            };
            let mut new_header = serde_json::to_string(&header)?;
            new_header.push('\n');

            // Atomic write: temp file → write header + rest → rename
            let tmp = path.with_extension("jsonl.tmp");
            std::fs::write(&tmp, new_header.as_bytes())?;
            let mut file = std::fs::OpenOptions::new().append(true).open(&tmp)?;
            file.write_all(rest.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &path)?;

            Ok(())
        })
    }

    /// Reset `total_tokens` in the header to zero (called after session_end).
    pub fn reset_header_tokens(&self, tree_id: &str) -> Result<()> {
        self.update_header(tree_id, &serde_json::json!({"total_tokens": 0}))
    }

    /// Scan all trees and return IDs whose last entry is not a `SessionEnd`.
    /// Used at server startup to detect trees that were interrupted by a crash.
    pub fn scan_unterminated(&self) -> Vec<String> {
        let dir = self.tree_dir();
        if !dir.exists() {
            return Vec::new();
        }
        let dir_entries: Vec<_> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
            Err(_) => return Vec::new(),
        };
        let mut result = Vec::new();
        for entry in dir_entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let tree_id = match path.file_name().and_then(|n| n.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let entries = self.read_all_entries(&tree_id).unwrap_or_default();
            match entries.last() {
                None | Some(Entry::SessionEnd { .. }) => {}
                _ => result.push(tree_id),
            }
        }
        result
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use tempfile::TempDir;

    fn make_store(name: &str) -> (Store, TempDir) {
        let dir = TempDir::with_prefix(&format!("agent-store-{}", name)).unwrap();
        let store = Store::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[test]
    fn test_create_tree_writes_subdir() {
        let (store, dir) = make_store("subdir");
        let tree_id = "subdir-001";

        store.create_tree_file(tree_id, "test-model").unwrap();

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
        assert!(tree_dir.join("data.jsonl").exists(), "data.jsonl should exist in tree subdir");
        assert!(tree_dir.join("meta.json").exists(), "meta.json should exist in tree subdir");
    }

    #[test]
    fn test_create_and_read_tree() {
        let (store, _dir) = make_store("create_read");
        let tree_id = "test-001";

        store.create_tree_file(tree_id, "test-model").unwrap();

        let entries = store.read_all_entries(tree_id).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_append_and_read_entries() {
        let (store, _dir) = make_store("append_read");
        let tree_id = "test-002";

        store.create_tree_file(tree_id, "test-model").unwrap();

        let entry = Entry::SessionStart {
            id: "00000001".into(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00Z".into(),
        };
        store.append_entry(tree_id, &entry).unwrap();

        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
        };
        let entry2 = Entry::Message {
            id: "00000002".into(),
            parent_id: Some("00000001".into()),
            timestamp: "2026-01-01T00:00:01Z".into(),
            message: msg,
        };
        store.append_entry(tree_id, &entry2).unwrap();

        let entries = store.read_all_entries(tree_id).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id(), "00000001");
        assert_eq!(entries[1].id(), "00000002");
    }

    #[test]
    fn test_tree_meta_roundtrip() {
        let (store, _dir) = make_store("meta_rt");
        let tree_id = "test-003";

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

        let loaded = store.load_tree_meta(tree_id).unwrap().unwrap();
        assert_eq!(loaded.id, tree_id);
        assert_eq!(loaded.title.unwrap(), "Test Tree");

        let cached = store.get_tree(tree_id).unwrap().unwrap();
        assert_eq!(cached.created_at, 1000);
    }

    #[test]
    fn test_list_and_rebuild() {
        let (store, _dir) = make_store("list_rebuild");
        let id1 = "list-001";
        let id2 = "list-002";

        let m1 = TreeMeta {
            id: id1.to_string(),
            parent_id: None,
            repo_path: None,
            title: Some("First".into()),
            created_at: 100,
            updated_at: 200,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        let m2 = TreeMeta {
            id: id2.to_string(),
            parent_id: None,
            repo_path: None,
            title: Some("Second".into()),
            created_at: 100,
            updated_at: 300,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        store.save_tree_meta(&m1).unwrap();
        store.save_tree_meta(&m2).unwrap();

        // Remove from global cache so rebuild must read from disk
        store.clear_cache(id1);
        store.clear_cache(id2);

        let rebuilt = store.rebuild_index().unwrap();
        assert!(rebuilt.iter().any(|m| m.id == id2), "rebuild should include id2");
        assert!(rebuilt.iter().any(|m| m.id == id1), "rebuild should include id1");
        // id2 has higher updated_at
        let pos_id2 = rebuilt.iter().position(|m| m.id == id2).unwrap();
        let pos_id1 = rebuilt.iter().position(|m| m.id == id1).unwrap();
        assert!(pos_id2 < pos_id1, "id2 (updated 300) should sort before id1 (updated 200)");
    }

    #[test]
    fn test_update_header() {
        let (store, _dir) = make_store("update_hdr");
        let tree_id = "test-004";

        store.create_tree_file(tree_id, "old-model").unwrap();

        store.update_header(tree_id, &serde_json::json!({
            "total_tokens": 500,
            "current_model": "new-model",
        })).unwrap();

        let content = std::fs::read_to_string(store.jsonl_path(tree_id)).unwrap();
        let first_line = content.lines().next().unwrap();
        let header: TreeHeader = serde_json::from_str(first_line).unwrap();
        assert_eq!(header.current_model, "new-model");
        assert_eq!(header.total_tokens, 500);
    }

    #[test]
    fn test_scan_unterminated() {
        let (store, _dir) = make_store("scan_unterm");
        let id1 = "unterm-001";
        let id2 = "unterm-002";
        let id3 = "unterm-003";

        // id1: no entries at all (header only)
        store.create_tree_file(id1, "model").unwrap();
        let meta1 = TreeMeta {
            id: id1.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        store.save_tree_meta(&meta1).unwrap();

        // id2: session_start + message, no session_end (unterminated)
        store.create_tree_file(id2, "model").unwrap();
        let meta2 = TreeMeta {
            id: id2.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        store.save_tree_meta(&meta2).unwrap();
        store.append_entry(id2, &Entry::SessionStart {
            id: "s1".into(), parent_id: None, timestamp: "t1".into(),
        }).unwrap();
        store.append_entry(id2, &Entry::Message {
            id: "m1".into(), parent_id: Some("s1".into()), timestamp: "t2".into(),
            message: Message { role: MessageRole::User, content: MessageContent::Text("hi".into()),
                tool_calls: None, tool_call_id: None, tool_name: None, usage: None,
                stop_reason: None, is_error: None },
        }).unwrap();

        // id3: session_start + session_end (properly terminated)
        store.create_tree_file(id3, "model").unwrap();
        let meta3 = TreeMeta {
            id: id3.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        store.save_tree_meta(&meta3).unwrap();
        store.append_entry(id3, &Entry::SessionStart {
            id: "s2".into(), parent_id: None, timestamp: "t3".into(),
        }).unwrap();
        store.append_entry(id3, &Entry::SessionEnd {
            id: "e1".into(), parent_id: Some("s2".into()), timestamp: "t4".into(),
            summary: Some("done".into()), status: SessionStatus::Completed,
            continuation_brief: None,
        }).unwrap();

        let unterm = store.scan_unterminated();
        assert!(unterm.contains(&id2.to_string()), "id2 should be unterminated");
        assert!(!unterm.contains(&id1.to_string()), "id1 (no entries) should not be unterminated");
        assert!(!unterm.contains(&id3.to_string()), "id3 (has session_end) should not be unterminated");
    }
}
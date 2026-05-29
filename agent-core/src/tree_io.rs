use std::path::{Path, PathBuf};

use log::warn;

use crate::types::{TreeHeader, TreeMeta};

#[derive(Debug, thiserror::Error)]
pub enum TreeIoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type TreeIoResult<T> = std::result::Result<T, TreeIoError>;

/// Returns ~/.agent/trees/{id} (or base/trees/{id} in tests).
pub fn tree_dir(base: &Path, tree_id: &str) -> PathBuf {
    base.join("trees").join(tree_id)
}

/// Read and parse meta.json. Returns None if the file doesn't exist.
pub async fn read_meta(base: &Path, tree_id: &str) -> TreeIoResult<Option<TreeMeta>> {
    let path = tree_dir(base, tree_id).join("meta.json");
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => serde_json::from_str(&content)
            .map(Some)
            .map_err(TreeIoError::Json),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(TreeIoError::Io(e)),
    }
}

/// Write meta.json atomically (write to .tmp, rename over target).
/// Creates the tree directory if it doesn't exist.
pub async fn write_meta(base: &Path, meta: &TreeMeta) -> TreeIoResult<()> {
    let dir = tree_dir(base, &meta.id);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("meta.json");
    let tmp = path.with_extension("meta.tmp");
    let content = serde_json::to_string_pretty(meta).map_err(TreeIoError::Json)?;
    tokio::fs::write(&tmp, &content).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Scan base/trees/*/meta.json and return all parseable TreeMetas,
/// sorted by updated_at descending. Logs and skips corrupt files.
pub async fn list_trees(base: &Path) -> TreeIoResult<Vec<TreeMeta>> {
    let dir = base.join("trees");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(TreeIoError::Io(e)),
    };
    let mut trees = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        match tokio::fs::read_to_string(&meta_path).await {
            Ok(content) => match serde_json::from_str::<TreeMeta>(&content) {
                Ok(meta) => trees.push(meta),
                Err(e) => warn!("Skipping corrupt meta file {:?}: {}", meta_path, e),
            },
            Err(e) => warn!("Skipping unreadable meta file {:?}: {}", meta_path, e),
        }
    }
    trees.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    Ok(trees)
}

/// Create a new tree: mkdir base/trees/{id}, write the data.jsonl header
/// line, write meta.json atomically.
pub async fn create_tree(base: &Path, meta: &TreeMeta) -> TreeIoResult<()> {
    let dir = tree_dir(base, &meta.id);
    tokio::fs::create_dir_all(&dir).await?;

    let header = TreeHeader {
        kind: "meta".to_string(),
        version: 1,
        id: meta.id.clone(),
    };
    let mut line = serde_json::to_string(&header).map_err(TreeIoError::Json)?;
    line.push('\n');
    tokio::fs::write(dir.join("data.jsonl"), line.as_bytes()).await?;

    write_meta(base, meta).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TreeSandbox;
    use tempfile::TempDir;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn test_create_tree_roundtrip() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();

        let meta = TreeMeta {
            id: "roundtrip-001".into(),
            parent_id: None,
            repo_path: None,
            title: Some("Test".into()),
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };

        block_on(create_tree(&base, &meta)).unwrap();

        // Verify data.jsonl header
        let header_path = tree_dir(&base, "roundtrip-001").join("data.jsonl");
        let content = std::fs::read_to_string(&header_path).unwrap();
        let header: TreeHeader = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(header.kind, "meta");
        assert_eq!(header.version, 1);
        assert_eq!(header.id, "roundtrip-001");

        // Verify meta.json roundtrip
        let loaded = block_on(read_meta(&base, "roundtrip-001")).unwrap().unwrap();
        assert_eq!(loaded.id, "roundtrip-001");
        assert_eq!(loaded.title.unwrap(), "Test");
    }

    #[test]
    fn test_write_meta_atomicity() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();

        // Create tree first
        let meta = TreeMeta {
            id: "atomic-001".into(),
            parent_id: None,
            repo_path: None,
            title: Some("First".into()),
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        block_on(create_tree(&base, &meta)).unwrap();

        // Verify .tmp is gone after write
        let tree_path = tree_dir(&base, "atomic-001");
        assert!(!tree_path.join("meta.meta.tmp").exists(), ".tmp file should be cleaned up");

        // Update title
        let mut updated = meta;
        updated.title = Some("Updated".into());
        updated.updated_at = 200;
        block_on(write_meta(&base, &updated)).unwrap();

        // Verify .tmp is gone
        assert!(!tree_path.join("meta.meta.tmp").exists());

        // Verify content
        let loaded = block_on(read_meta(&base, "atomic-001")).unwrap().unwrap();
        assert_eq!(loaded.title.unwrap(), "Updated");
        assert_eq!(loaded.updated_at, 200);
    }

    #[test]
    fn test_list_trees_sorts_by_updated_at() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();

        let m1 = TreeMeta {
            id: "list-001".into(),
            parent_id: None,
            repo_path: None,
            title: Some("First".into()),
            created_at: 100,
            updated_at: 200,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        let m2 = TreeMeta {
            id: "list-002".into(),
            parent_id: None,
            repo_path: None,
            title: Some("Second".into()),
            created_at: 100,
            updated_at: 300,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };

        block_on(create_tree(&base, &m1)).unwrap();
        block_on(create_tree(&base, &m2)).unwrap();

        let trees = block_on(list_trees(&base)).unwrap();
        assert_eq!(trees.len(), 2);
        // Second (updated_at=300) should sort before First (updated_at=200)
        assert_eq!(trees[0].id, "list-002");
        assert_eq!(trees[1].id, "list-001");
    }

    #[test]
    fn test_list_trees_skips_corrupt() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();

        // Write a valid tree
        let meta = TreeMeta {
            id: "valid-001".into(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        block_on(create_tree(&base, &meta)).unwrap();

        // Write corrupt meta.json in another tree dir
        let corrupt_dir = tree_dir(&base, "corrupt-001");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        std::fs::write(corrupt_dir.join("meta.json"), "not valid json").unwrap();

        let trees = block_on(list_trees(&base)).unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].id, "valid-001");
    }

    #[test]
    fn test_read_meta_nonexistent() {
        let dir = TempDir::new().unwrap();
        let result = block_on(read_meta(dir.path(), "no-such-tree")).unwrap();
        assert!(result.is_none());
    }
}

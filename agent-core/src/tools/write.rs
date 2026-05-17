//! WriteTool — write content to a file.

use std::fs;
use std::path::{Path, PathBuf};

use super::{Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description:
                "Write content to a file. Creates parent directories if needed. \
                 If the file exists, it is overwritten."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to repo root"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value) -> ToolResult {
        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: path".to_string())?;

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: content".to_string())?;

        let target = self.cwd.join(path_str);

        // Check path is within cwd by resolving ".." components.
        // We can't canonicalize() if the file doesn't exist, so we
        // canonicalize cwd and do a simplified path resolution.
        let cwd_canon = fs::canonicalize(&self.cwd)
            .map_err(|e| format!("Cannot resolve repo root: {}", e))?;

        // Resolve path components against cwd
        let target_normalized = path_str
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .fold(cwd_canon.clone(), |base, part| {
                if part == ".." {
                    base.parent().unwrap_or(&base).to_path_buf()
                } else {
                    base.join(part)
                }
            });

        if !target_normalized.starts_with(&cwd_canon) {
            return Err(format!(
                "Path '{}' resolves outside repo root ({})",
                path_str,
                target_normalized.display()
            )
            .into());
        }

        // Create parent directories if needed
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&target, content)?;

        let bytes = content.len();
        Ok(ToolOutput {
            content: format!(
                "Successfully wrote {} bytes to {}",
                bytes,
                target.display()
            ),
            truncated: false,
            original_size: bytes,
            exit_code: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_write_new_file() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "new.txt", "content": "hello world"}))
            .unwrap();
        assert!(result.content.contains("11 bytes"));

        let content = fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_write_creates_dirs() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "sub/dir/file.txt", "content": "test"}))
            .unwrap();
        assert!(result.content.contains("wrote"));
        let content = fs::read_to_string(dir.path().join("sub/dir/file.txt")).unwrap();
        assert_eq!(content, "test");
    }

    #[test]
    fn test_write_overwrites() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.txt"), "old").unwrap();
        let tool = WriteTool::new(dir.path());
        tool.execute(&serde_json::json!({"path": "existing.txt", "content": "new"}))
            .unwrap();
        let content = fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }
}
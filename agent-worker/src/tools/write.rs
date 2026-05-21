//! WriteTool — write content to a file.

use std::fs;

use super::{EditRecord, Tool, ToolContext, ToolResult};
use agent_core::types::{ToolDefinition, ToolOutput};

pub struct WriteTool;

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

    fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolResult {
        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: path".to_string())?;

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: content".to_string())?;

        let target = ctx.cwd.join(path_str);

        let cwd_canon = fs::canonicalize(&ctx.cwd)
            .map_err(|e| format!("Cannot resolve repo root: {}", e))?;

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

        // Attempt to read pre-existing content for the edit record.
        let pre_snapshot = fs::read_to_string(&target).ok();

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        let new_content = content.to_string();
        fs::write(&target, &new_content)?;

        let id = ctx.edit_store.insert(EditRecord {
            file_path: target.clone(),
            pre_snapshot,
            edits: vec![],
            post_snapshot: Some(new_content.clone()),
        });

        let line_count = new_content.lines().count();
        Ok(ToolOutput {
            content: format!(
                "edit_id: {}\nWritten: {} ({} lines)",
                id,
                target.display(),
                line_count
            ),
            truncated: false,
            original_size: new_content.len(),
            exit_code: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::fs;
    use tempfile::TempDir;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    #[test]
    fn test_write_new_file() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool;
        let mut ctx = make_ctx(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "new.txt", "content": "hello world"}), &mut ctx)
            .unwrap();
        assert!(result.content.contains("edit_id:"));
        assert!(result.content.contains("Written:"));

        let content = fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_write_creates_dirs() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool;
        let mut ctx = make_ctx(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "sub/dir/file.txt", "content": "test"}), &mut ctx)
            .unwrap();
        assert!(result.content.contains("edit_id:"));
        let content = fs::read_to_string(dir.path().join("sub/dir/file.txt")).unwrap();
        assert_eq!(content, "test");
    }

    #[test]
    fn test_write_overwrites() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.txt"), "old").unwrap();
        let tool = WriteTool;
        let mut ctx = make_ctx(dir.path());
        tool.execute(&serde_json::json!({"path": "existing.txt", "content": "new"}), &mut ctx)
            .unwrap();
        let content = fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }
}
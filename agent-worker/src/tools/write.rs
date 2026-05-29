//! WriteTool — write content to a file.

use std::fs;

use super::{EditRecord, Tool, ToolContext, ToolOutput};
use agent_core::types::ToolDefinition;

pub struct WriteTool;

impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: "Write content to a file. Creates parent directories if needed. \
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

    fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput {
        let path_str = match params.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::Done(Err("Missing required field: path".to_string())),
        };

        let content = match params.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::Done(Err("Missing required field: content".to_string())),
        };

        let target = ctx.cwd.join(path_str);

        let cwd_canon = match fs::canonicalize(&ctx.cwd) {
            Ok(p) => p,
            Err(e) => return ToolOutput::Done(Err(format!("Cannot resolve repo root: {}", e))),
        };

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
            return ToolOutput::Done(Err(format!(
                "Path '{}' resolves outside repo root ({})",
                path_str,
                target_normalized.display()
            )));
        }

        let pre_snapshot = fs::read_to_string(&target).ok();

        if let Some(parent) = target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return ToolOutput::Done(Err(e.to_string()));
            }
        }

        let new_content = content.to_string();
        if let Err(e) = fs::write(&target, &new_content) {
            return ToolOutput::Done(Err(e.to_string()));
        }

        ctx.lsp_dirty.push(target_normalized.clone());

        let id = ctx.edit_store.insert(EditRecord {
            file_path: target_normalized.clone(),
            pre_snapshot,
            edits: vec![],
            post_snapshot: Some(new_content.clone()),
            reverted: false,
        });

        let line_count = new_content.lines().count();
        ToolOutput::Done(Ok(format!(
            "edit_id: {}\nWritten: {} ({} lines)",
            id,
            target.display(),
            line_count
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    fn run_ok(tool: &WriteTool, params: serde_json::Value, ctx: &mut ToolContext) -> String {
        match tool.execute(&params, ctx) {
            ToolOutput::Done(Ok(c)) => c,
            _ => panic!("expected Done(Ok)"),
        }
    }

    #[test]
    fn test_write_new_file() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({"path": "new.txt", "content": "hello world"}),
            &mut ctx,
        );
        assert!(result.contains("edit_id:"));
        assert!(result.contains("Written:"));

        let content = fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_write_creates_dirs() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({"path": "sub/dir/file.txt", "content": "test"}),
            &mut ctx,
        );
        assert!(result.contains("edit_id:"));
        let content = fs::read_to_string(dir.path().join("sub/dir/file.txt")).unwrap();
        assert_eq!(content, "test");
    }

    #[test]
    fn test_write_overwrites() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.txt"), "old").unwrap();
        let tool = WriteTool;
        let mut ctx = make_ctx(dir.path());
        let result = tool.execute(
            &serde_json::json!({"path": "existing.txt", "content": "new"}),
            &mut ctx,
        );
        assert!(matches!(result, ToolOutput::Done(Ok(_))));
        let content = fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }
}

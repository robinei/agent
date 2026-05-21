//! ReadTool — read a file with line/byte limits.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{resolve_path, Tool, ToolResult};
use agent_core::types::{ToolDefinition, ToolOutput};

pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
            description:
                "Read the contents of a file. Shows line numbers and enforces a limit of \
                 2000 lines / 50 KB. If the file is large, use offset to read a portion."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to repo root"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Starting line number (1-indexed, default 1)",
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max lines to read (default 2000)",
                        "minimum": 1
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value, _stop: &Arc<AtomicBool>) -> ToolResult {
        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: path".to_string())?;

        let resolved = resolve_path(&self.cwd, path_str)
            .ok_or_else(|| format!("Path '{}' is outside repo root or does not exist", path_str))?;

        if !resolved.is_file() {
            return Err(format!("Not a file: {}", resolved.display()).into());
        }

        let offset: usize = params
            .get("offset")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(1) as usize)
            .unwrap_or(1);

        let max_lines: usize = params
            .get("limit")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(1) as usize)
            .unwrap_or(2000);

        // Read the file with line bounds
        let file = fs::File::open(&resolved)?;
        let reader = BufReader::new(file);
        let mut lines: Vec<String> = Vec::new();
        let mut total_size = 0usize;
        let max_bytes = 50 * 1024; // 50 KB

        for line_result in reader.lines().skip(offset - 1).take(max_lines + 1) {
            let line = line_result?;
            let line_bytes = line.len() + 1; // +1 for newline
            if total_size + line_bytes > max_bytes || lines.len() >= max_lines {
                break;
            }
            total_size += line_bytes;
            lines.push(line);
        }

        // Count total lines in file for reporting
        let file_content = fs::read_to_string(&resolved)?;
        let total_lines = file_content.lines().count();
        let original_size = file_content.len();

        let mut content = String::new();
        let mut truncated = false;

        if offset > total_lines {
            content = format!(
                "File has {} lines, offset {} is past end of file.\n",
                total_lines, offset
            );
        } else {
            let end_line = offset + lines.len() - 1;
            for (i, line) in lines.iter().enumerate() {
                content.push_str(&format!("{:>6} | {}\n", offset + i, line));
            }

            if end_line < total_lines {
                content.push_str(&format!(
                    "... (showing lines {}-{} of {})",
                    offset, end_line, total_lines
                ));
                truncated = true;
            }
        }

        if total_size > max_bytes {
            truncated = true;
            content.push_str(&format!(
                "\n[Output truncated: exceeded max size of {} KB]",
                max_bytes / 1024
            ));
        }

        Ok(ToolOutput {
            content,
            truncated,
            original_size,
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
    fn test_read_small_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "hello\nworld\n").unwrap();
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "test.txt"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("hello"));
        assert!(result.content.contains("world"));
        assert!(!result.truncated);
    }

    #[test]
    fn test_read_with_offset() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.txt"),
            "line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "test.txt", "offset": 3}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("line3"));
        assert!(result.content.contains("line4"));
        assert!(!result.content.contains("line1"));
    }

    #[test]
    fn test_read_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let tool = ReadTool::new(dir.path());
        let result =
            tool.execute(&serde_json::json!({"path": "nope.txt"}), &Arc::new(AtomicBool::new(false)));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_escape_rejected() {
        let dir = TempDir::new().unwrap();
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "../etc/passwd"}), &Arc::new(AtomicBool::new(false)));
        assert!(result.is_err());
    }
}

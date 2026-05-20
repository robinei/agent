//! LsTool — directory listing with file type and size info.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{resolve_path, Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Tool for LsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ls".to_string(),
            description:
                "List directory contents with file type, size, and permissions. \
                 Max 500 entries. Use for exploring project structure."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path relative to repo root (default: repo root)"
                    }
                },
                "required": []
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value, _stop: &Arc<AtomicBool>) -> ToolResult {
        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let resolved = if path_str.is_empty() {
            self.cwd.clone()
        } else {
            resolve_path(&self.cwd, path_str)
                .ok_or_else(|| format!("Path '{}' is outside repo root or does not exist", path_str))?
        };

        if !resolved.is_dir() {
            return Err(format!("Not a directory: {}", resolved.display()).into());
        }

        let max_entries: usize = 500;
        let mut entries = Vec::new();
        let mut truncated = false;

        let dir = fs::read_dir(&resolved)?;
        for entry in dir {
            if entries.len() >= max_entries {
                truncated = true;
                break;
            }
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            let metadata = entry.metadata().ok();

            let kind = metadata
                .as_ref()
                .map(|m| {
                    if m.is_dir() {
                        "dir"
                    } else if m.is_symlink() {
                        "link"
                    } else {
                        "file"
                    }
                })
                .unwrap_or("unknown");

            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let perms = metadata
                .as_ref()
                .map(|m| {
                    let mode = m.mode();
                    format!(
                        "{}{}{}{}{}{}{}{}{}",
                        if mode & 0o400 != 0 { "r" } else { "-" },
                        if mode & 0o200 != 0 { "w" } else { "-" },
                        if mode & 0o100 != 0 { "x" } else { "-" },
                        if mode & 0o040 != 0 { "r" } else { "-" },
                        if mode & 0o020 != 0 { "w" } else { "-" },
                        if mode & 0o010 != 0 { "x" } else { "-" },
                        if mode & 0o004 != 0 { "r" } else { "-" },
                        if mode & 0o002 != 0 { "w" } else { "-" },
                        if mode & 0o001 != 0 { "x" } else { "-" },
                    )
                })
                .unwrap_or_else(|| "?????????".to_string());

            let size_str = if kind == "dir" {
                String::new()
            } else if size < 1024 {
                format!("{:>4}B", size)
            } else if size < 1024 * 1024 {
                format!("{:>4}K", size / 1024)
            } else {
                format!("{:>4}M", size / (1024 * 1024))
            };

            entries.push(format!("{} {:>6} {} {}", perms, size_str, kind, name_str));
        }

        let original_size = entries.len();
        entries.sort();

        let mut content = String::new();
        content.push_str(&format!(
            "{} ({} entries):\n",
            resolved.display(),
            if truncated {
                format!("{}", original_size)
            } else {
                original_size.to_string()
            }
        ));
        for line in &entries {
            content.push_str(line);
            content.push('\n');
        }

        if truncated {
            content.push_str(&format!(
                "\n... (showing {} of {} entries, max {})",
                entries.len(),
                original_size,
                max_entries
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
    fn test_ls_directory() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.txt"), "world").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let tool = LsTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("b.txt"));
        assert!(result.content.contains("sub"));
    }

    #[test]
    fn test_ls_subdirectory() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("nested.txt"), "test").unwrap();
        let tool = LsTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "sub"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("nested.txt"));
    }
}
//! FindTool — file/directory search using walkdir.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use walkdir::WalkDir;

use super::{resolve_path, Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

/// Directories to skip during recursive search.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".hg", ".svn", "vendor"];

pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    /// Check if the filename matches a glob-like pattern.
    /// Supports: `*.rs`, `*test*`, `main*`, `*main.rs`
    fn matches_pattern(name: &str, pattern: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        if pattern.contains('*') {
            let parts: Vec<&str> = pattern.split('*').collect();
            let mut pos = 0usize;
            for (i, part) in parts.iter().enumerate() {
                if part.is_empty() {
                    continue;
                }
                if i == 0 {
                    // Must start with first non-empty part
                    if !name.starts_with(part) {
                        return false;
                    }
                    pos = part.len();
                } else if i == parts.len() - 1 {
                    // Last non-empty part — must end with it
                    if !name[pos..].ends_with(part) {
                        return false;
                    }
                    pos = name.len();
                } else {
                    // Middle part — must contain it somewhere after pos
                    let remaining = &name[pos..];
                    if let Some(found) = remaining.find(part) {
                        pos += found + part.len();
                    } else {
                        return false;
                    }
                }
            }
            true
        } else {
            // No wildcard — substring match (case-insensitive)
            name.to_lowercase().contains(&pattern.to_lowercase())
        }
    }
}

impl Tool for FindTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find".to_string(),
            description:
                "Search for files and directories by name pattern. Supports glob patterns \
                 (e.g., \"*.rs\", \"*test*\"). Max 500 results. Skips .git/, node_modules/, \
                 target/ by default."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob or substring pattern to match filenames against"
                    },
                    "path": {
                        "type": "string",
                        "description": "Subdirectory to search (default: entire repo)"
                    },
                    "type": {
                        "type": "string",
                        "description": "Filter by type: \"file\", \"dir\", or \"both\" (default: both)",
                        "enum": ["file", "dir", "both"]
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results (default 500)",
                        "minimum": 1,
                        "maximum": 10000
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value, _stop: &Arc<AtomicBool>) -> ToolResult {
        let pattern_str = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: pattern".to_string())?;

        let max_results: usize = params
            .get("max_results")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(1) as usize)
            .unwrap_or(500);

        let filter_type = params
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("both");

        let search_dir = if let Some(sub_path) = params.get("path").and_then(|v| v.as_str()) {
            if sub_path.is_empty() {
                self.cwd.clone()
            } else {
                resolve_path(&self.cwd, sub_path)
                    .ok_or_else(|| format!("Path '{}' is outside repo root", sub_path))?
            }
        } else {
            self.cwd.clone()
        };

        let mut results: Vec<String> = Vec::new();
        let mut truncated = false;

        for entry in WalkDir::new(&search_dir)
            .into_iter()
            .filter_entry(|e| {
                !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
            })
        {
            if results.len() >= max_results {
                truncated = true;
                break;
            }
            let entry = entry?;

            // Apply type filter
            match filter_type {
                "file" if !entry.file_type().is_file() => continue,
                "dir" if !entry.file_type().is_dir() => continue,
                _ => {} // "both" or matches type — no filter
            }

            let name = entry.file_name().to_string_lossy();
            if Self::matches_pattern(&name, pattern_str) {
                let relative = entry
                    .path()
                    .strip_prefix(&self.cwd)
                    .unwrap_or(entry.path());
                let kind = if entry.file_type().is_dir() {
                    "dir "
                } else {
                    "file"
                };
                results.push(format!("{} {}", kind, relative.display()));
            }
        }

        let original_size = results.len();
        results.sort();

        let mut content = String::new();
        if results.is_empty() {
            content = format!(
                "No files found matching pattern '{}' in {}\n",
                pattern_str,
                search_dir.display()
            );
        } else {
            for line in &results {
                content.push_str(line);
                content.push('\n');
            }
            if truncated {
                content.push_str(&format!(
                    "\n... (showing {} of {} results, max {})",
                    results.len(),
                    original_size,
                    max_results
                ));
            }
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
    fn test_find_by_extension() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.rs"), "").unwrap();
        fs::write(dir.path().join("lib.rs"), "").unwrap();
        fs::write(dir.path().join("readme.md"), "").unwrap();
        let tool = FindTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "*.rs"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("main.rs"));
        assert!(result.content.contains("lib.rs"));
        assert!(!result.content.contains("readme.md"));
    }

    #[test]
    fn test_find_substring() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test_main.rs"), "").unwrap();
        fs::write(dir.path().join("main_test.rs"), "").unwrap();
        fs::write(dir.path().join("readme.md"), "").unwrap();
        let tool = FindTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "test"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("test_main.rs"));
        assert!(result.content.contains("main_test.rs"));
        assert!(!result.content.contains("readme.md"));
    }

    #[test]
    fn test_find_dirs_only() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::create_dir(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        let tool = FindTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "*", "type": "dir"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("src"));
        assert!(result.content.contains("tests"));
        assert!(!result.content.contains("file.txt"));
    }

    #[test]
    fn test_find_no_match() {
        let dir = TempDir::new().unwrap();
        let tool = FindTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "zzzzz"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("No files found"));
    }
}
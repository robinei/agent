//! GrepTool — recursive file content search using regex.
//!
//! Skips binary files and common non-source directories (.git, node_modules, target).

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

use super::{resolve_path, Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

/// Directories to skip during recursive search.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".hg", ".svn", "vendor"];

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    /// Check if a path looks like a binary file by looking at first few bytes.
    fn is_binary(path: &Path) -> bool {
        if let Ok(content) = fs::read(path) {
            // Check for null bytes in first 8KB — strong indicator of binary
            let check_len = content.len().min(8192);
            content[..check_len].contains(&0)
        } else {
            true
        }
    }
}

impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description:
                "Recursive file content search using regex. Returns up to 100 matches \
                 with optional context lines. Skips binary files and .git/, node_modules/, \
                 target/ by default. Use for finding patterns across the codebase."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Subdirectory or file glob to restrict search (default: entire repo)"
                    },
                    "max_matches": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (default 100)",
                        "minimum": 1,
                        "maximum": 1000
                    },
                    "context_lines": {
                        "type": "integer",
                        "description": "Lines of context before/after each match (default 0)",
                        "minimum": 0,
                        "maximum": 15
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value) -> ToolResult {
        let pattern_str = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: pattern".to_string())?;

        let re = Regex::new(pattern_str)
            .map_err(|e| format!("Invalid regex '{}': {}", pattern_str, e))?;

        let max_matches: usize = params
            .get("max_matches")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(1) as usize)
            .unwrap_or(100);

        let context_lines: usize = params
            .get("context_lines")
            .and_then(|v| v.as_i64())
            .map(|v| v.max(0) as usize)
            .unwrap_or(0)
            .min(15);

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
        let mut match_count = 0;
        let mut files_searched = 0usize;
    

        for entry in WalkDir::new(&search_dir)
            .into_iter()
            .filter_entry(|e| {
                !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
            })
        {
            if match_count >= max_matches {
                break;
            }
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            if Self::is_binary(path) {
                continue;
            }

            files_searched += 1;

            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue, // skip unreadable files
            };

            let relative = path
                .strip_prefix(&self.cwd)
                .unwrap_or(path);

            let lines: Vec<&str> = content.lines().collect();
            for (lineno, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    if match_count >= max_matches {
                        break;
                    }
                    match_count += 1;

                    // Context lines before
                    let ctx_start = if context_lines > 0 {
                        lineno.saturating_sub(context_lines)
                    } else {
                        lineno
                    };
                    // Context lines after
                    let ctx_end = (lineno + 1 + context_lines).min(lines.len());

                    let mut block = String::new();

                    // Separator between results from different files
                    if results.is_empty()
                        || !results
                            .last()
                            .is_some_and(|r| r.starts_with(&format!("{}:", relative.display())))
                    {
                        block.push_str(&format!("{}:\n", relative.display()));
                    }

                    for ctx_lineno in ctx_start..ctx_end {
                        let prefix = if ctx_lineno == lineno {
                            ">"
                        } else {
                            " "
                        };
                        block.push_str(&format!(
                            "{} {:>6}: {}\n",
                            prefix,
                            ctx_lineno + 1,
                            lines[ctx_lineno]
                        ));
                    }

                    // Add a blank line separator between matches
                    if ctx_end < lines.len() {
                        block.push('\n');
                    }

                    results.push(block);
                }
            }
        }

        let original_size = match_count;
        let truncated = match_count > max_matches;

        let mut content = String::new();
        if results.is_empty() {
            content = format!(
                "No matches found for pattern '{}' in {} files searched.\n",
                pattern_str, files_searched
            );
        } else {
            for block in &results {
                content.push_str(block);
            }
            if truncated {
                content.push_str(&format!(
                    "\n... (showing {} of {} matches, max {})",
                    match_count, original_size, max_matches
                ));
            }
            content.push_str(&format!(
                "\n--- {} files searched, {} matches ---\n",
                files_searched, match_count
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
    fn test_grep_basic() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "hello world\nfoo bar\nhello again\n").unwrap();
        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "hello"}))
            .unwrap();
        assert!(result.content.contains("hello world"));
        assert!(result.content.contains("hello again"));
        assert!(!result.truncated);
    }

    #[test]
    fn test_grep_no_match() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();
        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "zzzz"}))
            .unwrap();
        assert!(result.content.contains("No matches"));
    }

    #[test]
    fn test_grep_with_context() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.txt"),
            "line1 before\nline2 match\nline3 after\n",
        )
        .unwrap();
        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "match", "context_lines": 1}))
            .unwrap();
        assert!(result.content.contains("line1 before"));
        assert!(result.content.contains("line2 match"));
        assert!(result.content.contains("line3 after"));
    }

    #[test]
    fn test_grep_in_subdirectory() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("nested.txt"), "secret\n").unwrap();
        let tool = GrepTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"pattern": "secret", "path": "sub"}))
            .unwrap();
        assert!(result.content.contains("nested.txt"));
    }
}
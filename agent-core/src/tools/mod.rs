//! Tool system: trait, registry, and implementations.
//!
//! Each tool lives in its own file and implements the `Tool` trait.
//! `all_tools()` collects them into a single registry.
//!
//! Tools use real filesystem calls directly (no generic I/O trait).
//! Tests create temp directories with fixture files.

use std::path::Path;

pub mod bash;
pub mod edit;
pub mod find;
pub mod git;
pub mod grep;
pub mod ls;
pub mod read;
pub mod search;
pub mod write;

use crate::types::{ToolDefinition, ToolOutput};

/// Result type for tool execution.
pub type ToolResult = Result<ToolOutput, Box<dyn std::error::Error + Send + Sync>>;

/// A tool that the LLM can invoke.
///
/// Tools are synchronous, Send, and use real filesystem calls.
pub trait Tool: Send {
    /// Returns the tool's JSON Schema definition (sent to the LLM).
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with the given JSON parameters.
    fn execute(&self, params: &serde_json::Value) -> ToolResult;
}

/// Register all available tools.
///
/// `cwd` is the working directory (repo root) that tools operate within.
/// Tools enforce path safety: they resolve all paths relative to `cwd`
/// and reject paths escaping the repo root via `..` or symlinks.
pub fn all_tools(cwd: &Path) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(bash::BashTool::new(cwd)),
        Box::new(edit::EditTool::new(cwd)),
        Box::new(find::FindTool::new(cwd)),
        Box::new(git::GitTool::new(cwd)),
        Box::new(grep::GrepTool::new(cwd)),
        Box::new(ls::LsTool::new(cwd)),
        Box::new(read::ReadTool::new(cwd)),
        Box::new(search::SearchMessagesTool::new(cwd)),
        Box::new(search::SearchFilesTool::new(cwd)),
        Box::new(write::WriteTool::new(cwd)),
    ]
}

/// Resolve a file path relative to `cwd`, rejecting paths that escape.
///
/// Returns `None` if the resolved path is outside `cwd` (prevents
/// directory traversal attacks / accidental escapes).
pub fn resolve_path(cwd: &Path, requested: &str) -> Option<std::path::PathBuf> {
    let joined = if requested.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(requested)
    };
    let canonical = std::fs::canonicalize(&joined).ok()?;
    let cwd_canonical = std::fs::canonicalize(cwd).ok()?;
    if canonical.starts_with(&cwd_canonical) {
        Some(canonical)
    } else {
        None
    }
}

/// Truncate output to the given limits, setting `truncated` if exceeded.
pub fn truncate_output(content: &str, max_lines: usize, max_bytes: usize) -> ToolOutput {
    let original_size = content.len();
    let mut truncated = false;
    let result = if content.len() > max_bytes {
        truncated = true;
        let mut s = content[..max_bytes].to_string();
        s.push_str("\n\n[Output truncated: exceeded max size]");
        s
    } else {
        // Check line count
        let line_count = content.lines().count();
        if line_count > max_lines {
            truncated = true;
            let mut s: String = content.lines().take(max_lines).collect::<Vec<_>>().join("\n");
            s.push_str(&format!(
                "\n\n[Output truncated: {} lines (limit {})]",
                line_count, max_lines
            ));
            s
        } else {
            content.to_string()
        }
    };
    ToolOutput {
        content: result,
        truncated,
        original_size,
        exit_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_resolve_path_within_cwd() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let resolved = resolve_path(dir.path(), "test.txt");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("test.txt"));
    }

    #[test]
    fn test_resolve_path_escape_rejected() {
        let dir = TempDir::new().unwrap();
        let resolved = resolve_path(dir.path(), "../etc/passwd");
        assert!(resolved.is_none());
    }

    #[test]
    fn test_truncate_output_bytes() {
        let result = truncate_output("hello world", 1000, 5);
        assert!(result.truncated);
        assert_eq!(result.original_size, 11);
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn test_truncate_output_lines() {
        let content = "line1\nline2\nline3\nline4\n";
        let result = truncate_output(content, 2, 10000);
        assert!(result.truncated);
        assert_eq!(result.original_size, content.len());
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line2"));
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn test_no_truncation() {
        let content = "short text";
        let result = truncate_output(content, 100, 10000);
        assert!(!result.truncated);
        assert_eq!(result.content, content);
    }
}

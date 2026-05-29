//! Tool system: trait, registry, and implementations.
//!
//! Each tool lives in its own file and implements the `Tool` trait.
//! `all_tools()` collects them into a single registry.
//!
//! Tools use real filesystem calls directly (no generic I/O trait).
//! Tests create temporary directories with fixture files.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub mod bash;
pub mod edit;
pub mod read;
pub mod restore_edit;
pub mod search;
pub mod util;
pub mod write;

use agent_core::types::ToolDefinition;

/// Result type for tool execution (async).
pub type ToolResult =
    Result<agent_core::types::ToolOutput, Box<dyn std::error::Error + Send + Sync>>;

/// Tool execution output: either a completed result or a pending LSP request.
pub enum ToolOutput {
    Done(Result<String, String>),
    PendingLsp { request_id: u64, lang_id: String },
}

/// Shared context passed to every tool execution.
pub struct ToolContext {
    pub cwd: PathBuf,
    pub edit_store: EditStore,
    pub stop: Arc<AtomicBool>,
    pub lsp_dirty: Vec<PathBuf>,

}

impl ToolContext {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            edit_store: EditStore::new(),
            stop: Arc::new(AtomicBool::new(false)),
            lsp_dirty: Vec::new(),

        }
    }
}

/// Thread-safe record of edits for the `restore_edit` tool.
#[derive(Clone)]
pub struct EditRecord {
    pub file_path: PathBuf,
    /// Full file content before the operation. `None` = file did not exist.
    pub pre_snapshot: Option<String>,
    /// (old_string, new_string) in application order. Empty for write records.
    pub edits: Vec<(String, String)>,
    /// Full file content after the operation. `Some` for write records;
    /// `None` for edits (computed on demand from pre_snapshot + edits).
    pub post_snapshot: Option<String>,
    /// Whether this record has been reverted. Prevents double-revert corruption.
    pub reverted: bool,
}

pub struct EditStore {
    next_id: u64,
    records: HashMap<u64, EditRecord>,
}

impl EditStore {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            records: HashMap::new(),
        }
    }

    pub fn insert(&mut self, record: EditRecord) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.records.insert(id, record);
        id
    }

    pub fn get(&self, id: u64) -> Option<&EditRecord> {
        self.records.get(&id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut EditRecord> {
        self.records.get_mut(&id)
    }
}

/// A tool that the LLM can invoke.
///
/// Tools are async, Send, and use tokio::fs / tokio::process.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Returns the tool's JSON Schema definition (sent to the LLM).
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with the given JSON parameters and shared context.
    async fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput;

    /// Resume a tool that was paused for an LSP response.
    async fn resume(
        &self,
        _response: serde_json::Value,
        _ctx: &mut ToolContext,
    ) -> Result<String, String> {
        unreachable!("tool '{}' does not implement resume", self.name())
    }

    /// Tool name, defaults to definition().name.
    fn name(&self) -> String {
        self.definition().name
    }
}

/// Register all available tools.
pub fn all_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(bash::BashTool),
        Box::new(edit::EditTool),
        Box::new(read::ReadTool),
        Box::new(restore_edit::RestoreEditTool),
        Box::new(search::SearchMessagesTool),
        Box::new(write::WriteTool),
    ]
}

/// Resolve a file path relative to `cwd`, rejecting paths that escape.
///
/// Returns `None` if the resolved path is outside `cwd` (prevents
/// directory traversal attacks / accidental escapes).
pub async fn resolve_path(cwd: &Path, requested: &str) -> Result<std::path::PathBuf, String> {
    let joined = if requested.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(requested)
    };
    let cwd_canonical = tokio::fs::canonicalize(cwd)
        .await
        .map_err(|e| format!("Cannot resolve repo root: {}", e))?;

    // Lexical escape check: resolves `..` without filesystem access so that
    // traversal attempts against non-existent targets still get the right error.
    if !normalize_path_lexical(&joined).starts_with(&cwd_canonical) {
        return Err(format!(
            "Path '{}' is outside the repo root ({})",
            requested,
            cwd_canonical.display()
        ));
    }

    // Canonicalize to resolve symlinks (also confirms the path exists).
    let canonical = tokio::fs::canonicalize(&joined)
        .await
        .map_err(|_| format!("Path '{}' does not exist", requested))?;

    // Re-check after symlink resolution to prevent symlink escapes.
    if !canonical.starts_with(&cwd_canonical) {
        return Err(format!(
            "Path '{}' is outside the repo root ({})",
            requested,
            cwd_canonical.display()
        ));
    }

    Ok(canonical)
}

fn normalize_path_lexical(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            c => result.push(c),
        }
    }
    result
}

/// Truncate output to the given limits, setting `truncated` if exceeded.
pub fn truncate_output(
    content: &str,
    max_lines: usize,
    max_bytes: usize,
) -> agent_core::types::ToolOutput {
    let original_size = content.len();
    let mut truncated = false;
    let result = if content.len() > max_bytes {
        truncated = true;
        let mut s = content[..max_bytes].to_string();
        s.push_str("\n\n[Output truncated: exceeded max size]");
        s
    } else {
        let line_count = content.lines().count();
        if line_count > max_lines {
            truncated = true;
            let mut s: String = content
                .lines()
                .take(max_lines)
                .collect::<Vec<_>>()
                .join("\n");
            s.push_str(&format!(
                "\n\n[Output truncated: {} lines (limit {})]",
                line_count, max_lines
            ));
            s
        } else {
            content.to_string()
        }
    };
    agent_core::types::ToolOutput {
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

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn test_resolve_path_within_cwd() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let resolved = block_on(resolve_path(dir.path(), "test.txt"));
        assert!(resolved.is_ok());
        assert!(resolved.unwrap().ends_with("test.txt"));
    }

    #[test]
    fn test_resolve_path_escape_rejected() {
        let dir = TempDir::new().unwrap();
        let resolved = block_on(resolve_path(dir.path(), "../etc/passwd"));
        assert!(resolved.is_err());
        assert!(resolved.unwrap_err().contains("outside the repo root"));
    }

    #[test]
    fn test_resolve_path_not_found() {
        let dir = TempDir::new().unwrap();
        let resolved = block_on(resolve_path(dir.path(), "nonexistent.txt"));
        assert!(resolved.is_err());
        assert!(resolved.unwrap_err().contains("does not exist"));
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

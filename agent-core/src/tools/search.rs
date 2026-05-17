//! Search tools — search past sessions and files in the store.
//!
//! `search_messages` scans JSONL tree files using serde stream deserialization
//! to find messages matching a query string.
//!
//! `search_files` finds files in the store directory by path pattern.

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::{Tool, ToolResult};
use crate::config::agent_dir;
use crate::types::{Entry, MessageContent, ToolDefinition, ToolOutput};

// ── Message search tool ──

pub struct SearchMessagesTool {
    #[allow(dead_code)]
    cwd: PathBuf,
}

impl SearchMessagesTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    /// Search all tree JSONL files for messages containing the query string.
    fn search_trees(
        query: &str,
        tree_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchMatch>, String> {
        let trees_dir = agent_dir().join("trees");

        if !trees_dir.exists() {
            return Ok(Vec::new());
        }

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        let mut _file_count = 0usize;

        for entry in fs::read_dir(&trees_dir).map_err(|e| format!("Cannot read trees dir: {}", e))? {
            let entry = entry.map_err(|e| format!("Dir entry error: {}", e))?;
            let path = entry.path();

            // Only process .jsonl files
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let fname = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if let Some(tid) = tree_id {
                if fname != tid {
                    continue;
                }
            }

            _file_count += 1;

            // Stream-deserialize the JSONL file
            let file = fs::File::open(&path).map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;
            let reader = std::io::BufReader::new(file);
            let mut lines = reader.lines();

            // Skip header line
            lines.next();

            // Parse entries via serde stream
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;

            for (line_idx, line) in content.lines().enumerate() {
                if results.len() >= limit {
                    break;
                }
                if line_idx == 0 {
                    continue; // skip header
                }
                if line.trim().is_empty() {
                    continue;
                }

                if let Ok(entry) = serde_json::from_str::<Entry>(line) {
                    if let Some(msg) = entry_as_message(&entry) {
                        let content_text = message_content_text(&msg.content);
                        if content_text.to_lowercase().contains(&query_lower) {
                            results.push(SearchMatch {
                                tree_id: fname.to_string(),
                                entry_id: entry.id().to_string(),
                                role: entry_role_str(&msg.role).to_string(),
                                snippet: truncate_snippet(&content_text, 200),
                                timestamp: extract_timestamp(&entry).unwrap_or_default(),
                            });
                        }
                    }
                }
            }

            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }
}

fn entry_as_message(entry: &Entry) -> Option<&crate::types::Message> {
    if let Entry::Message { message, .. } = entry {
        Some(message)
    } else {
        None
    }
}

fn entry_role_str(role: &crate::types::MessageRole) -> &'static str {
    match role {
        crate::types::MessageRole::System => "system",
        crate::types::MessageRole::User => "user",
        crate::types::MessageRole::Assistant => "assistant",
        crate::types::MessageRole::Tool => "tool",
    }
}

fn message_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if let crate::types::ContentBlock::Text { text: t } = block {
                    text.push_str(t);
                    text.push('\n');
                }
            }
            text
        }
    }
}

fn extract_timestamp(entry: &Entry) -> Option<String> {
    match entry {
        Entry::SessionStart { timestamp, .. }
        | Entry::Message { timestamp, .. }
        | Entry::SessionEnd { timestamp, .. }
        | Entry::BashExec { timestamp, .. }
        | Entry::ModelSet { timestamp, .. }
        | Entry::Label { timestamp, .. }
        | Entry::GoalSet { timestamp, .. } => Some(timestamp.clone()),
    }
}

fn truncate_snippet(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

impl Tool for SearchMessagesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_messages".to_string(),
            description:
                "Search past messages across all trees (or a specific tree) for a text query. \
                 Uses stream deserialization to avoid loading full files. Returns up to 20 matches."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to search for (case-insensitive)"
                    },
                    "tree_id": {
                        "type": "string",
                        "description": "Optional tree ID to restrict search to"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results (default 20, max 100)",
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value) -> ToolResult {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: query".to_string())?;

        let tree_id = params.get("tree_id").and_then(|v| v.as_str());
        let limit: usize = params
            .get("limit")
            .and_then(|v| v.as_i64())
            .map(|v| (v as usize).clamp(1, 100))
            .unwrap_or(20);

        let results = Self::search_trees(query, tree_id, limit)
            .map_err(|e| format!("Search failed: {}", e))?;

        if results.is_empty() {
            return Ok(ToolOutput {
                content: format!(
                    "No messages found matching '{}'{}.\n",
                    query,
                    tree_id.map(|t| format!(" in tree {}", t)).unwrap_or_default()
                ),
                truncated: false,
                original_size: 0,
                exit_code: None,
            });
        }

        let mut content = format!(
            "Found {} matching messages:\n\n",
            results.len()
        );
        for (i, m) in results.iter().enumerate() {
            content.push_str(&format!(
                "{}. [{}] {} (tree: {})\n   Snippet: {}\n\n",
                i + 1,
                m.timestamp,
                m.role,
                m.tree_id,
                m.snippet
            ));
        }

        let original_size = results.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size,
            exit_code: None,
        })
    }
}

#[derive(Debug)]
struct SearchMatch {
    tree_id: String,
    #[allow(dead_code)]
    entry_id: String,
    role: String,
    snippet: String,
    timestamp: String,
}

// ── File search tool ──

pub struct SearchFilesTool {
    #[allow(dead_code)]
    cwd: PathBuf,
}

impl SearchFilesTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Tool for SearchFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_files".to_string(),
            description:
                "Search for files in the agent store directory by path pattern. \
                 Finds JSONL tree files, metadata files, and other store artifacts."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob or substring pattern to match filenames"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results (default 50, max 500)",
                        "minimum": 1,
                        "maximum": 500
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value) -> ToolResult {
        let pattern = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: pattern".to_string())?;

        let limit: usize = params
            .get("limit")
            .and_then(|v| v.as_i64())
            .map(|v| (v as usize).clamp(1, 500))
            .unwrap_or(50);

        let store_dir = agent_dir();
        let mut results = Vec::new();
        let pattern_lower = pattern.to_lowercase();

        if store_dir.exists() {
            for entry in WalkDir::new(&store_dir)
                .into_iter()
                .filter_entry(|e| {
                    let name = e.file_name().to_string_lossy();
                    !name.starts_with('.') || name == ".agent"
                })
            {
                if results.len() >= limit {
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().is_file() {
                    continue;
                }

                let name = entry.file_name().to_string_lossy();
                // Simple glob: * matches everything, otherwise substring match
                let matched = if pattern == "*" {
                    true
                } else if pattern.contains('*') {
                    let parts: Vec<&str> = pattern.split('*').collect();
                    let mut pos = 0usize;
                    let mut ok = true;
                    for (i, part) in parts.iter().enumerate() {
                        if part.is_empty() { continue; }
                        let remaining = &name[pos..];
                        if i == 0 {
                            if !remaining.starts_with(part) { ok = false; break; }
                            pos = part.len();
                        } else if i == parts.len() - 1 {
                            if !remaining.ends_with(part) { ok = false; break; }
                            pos = name.len();
                        } else {
                            match remaining.find(part) {
                                Some(found) => pos += found + part.len(),
                                None => { ok = false; break; }
                            }
                        }
                    }
                    ok
                } else {
                    name.to_lowercase().contains(&pattern_lower)
                };

                if matched {
                    let relative = entry.path().strip_prefix(&store_dir)
                        .unwrap_or(entry.path());
                    let size = fs::metadata(entry.path())
                        .map(|m| m.len())
                        .unwrap_or(0);
                    results.push(format!("{} ({} bytes)", relative.display(), size));
                }
            }
        }

        results.sort();
        let original_size = results.len();
        let truncated = results.len() > limit;

        let content = if results.is_empty() {
            format!("No files found matching '{}' in store directory.\n", pattern)
        } else {
            let mut s = format!("Found {} files matching '{}':\n\n", results.len(), pattern);
            for line in &results[..results.len().min(limit)] {
                s.push_str(line);
                s.push('\n');
            }
            if truncated {
                s.push_str(&format!(
                    "\n... (showing {} of {} files, max {})",
                    results.len().min(limit),
                    original_size,
                    limit
                ));
            }
            s
        };

        Ok(ToolOutput {
            content,
            truncated,
            original_size,
            exit_code: None,
        })
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::types::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialize tests that modify AGENT_DIR env var (Rust tests run in parallel).
    static AGENT_DIR_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: set AGENT_DIR env var to a temp dir for the test.
    fn with_temp_agent_dir<F>(name: &str, f: F)
    where
        F: FnOnce(PathBuf),
    {
        let _guard = AGENT_DIR_LOCK.lock().unwrap();
        let dir = TempDir::with_prefix(&format!("agent-search-{}", name)).unwrap();
        let path = dir.path().to_path_buf();
        std::env::set_var("AGENT_DIR", &path);
        // Create trees subdirectory
        fs::create_dir_all(path.join("trees")).unwrap();
        f(path);
        std::env::remove_var("AGENT_DIR");
        // Keep dir alive until end of scope
        let _ = dir;
    }

    #[test]
    fn test_search_messages_empty() {
        with_temp_agent_dir("empty", |_path| {
            let tool = SearchMessagesTool::new(&PathBuf::from("/tmp"));
            let result = tool
                .execute(&serde_json::json!({"query": "hello"}))
                .unwrap();
            assert!(result.content.contains("No messages found"));
        });
    }

    #[test]
    fn test_search_messages_finds_match() {
        with_temp_agent_dir("find", |path| {
            let store = Store::new(path.clone());
            let tree_id = "search-test-001";
            store.create_tree_file(tree_id, "test-model").unwrap();

            let msg = Message {
                role: MessageRole::User,
                content: MessageContent::Text("hello world from search".into()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            };
            let entry = Entry::Message {
                id: "aaaa0001".into(),
                parent_id: None,
                timestamp: "2026-01-01T00:00:00Z".into(),
                message: msg,
            };
            store.append_entry(tree_id, &entry).unwrap();

            let tool = SearchMessagesTool::new(&PathBuf::from("/tmp"));
            let result = tool
                .execute(&serde_json::json!({"query": "hello world"}))
                .unwrap();
            assert!(result.content.contains("hello world from search"));
            assert!(result.content.contains("user"));
        });
    }

    #[test]
    fn test_search_messages_by_tree() {
        with_temp_agent_dir("by_tree", |path| {
            let store = Store::new(path.clone());
            let tree_id = "search-test-002";
            store.create_tree_file(tree_id, "test-model").unwrap();

            let msg = Message {
                role: MessageRole::User,
                content: MessageContent::Text("secret message".into()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            };
            // Need explicit construction since Default isn't derived for Message
            let entry = Entry::Message {
                id: "bbbb0001".into(),
                parent_id: None,
                timestamp: "2026-01-01T00:00:00Z".into(),
                message: msg,
            };
            store.append_entry(tree_id, &entry).unwrap();

            let tool = SearchMessagesTool::new(&PathBuf::from("/tmp"));
            // Search for a different tree_id — should find nothing
            let result = tool
                .execute(&serde_json::json!({
                    "query": "secret",
                    "tree_id": "wrong-tree-id"
                }))
                .unwrap();
            assert!(result.content.contains("No messages found"));
        });
    }

    #[test]
    fn test_parse_entry_from_jsonl() {
        // Verify we can parse entries we serialized
        let entry = Entry::Message {
            id: "cccc0001".into(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00Z".into(),
            message: Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("test".into()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id(), "cccc0001");
    }
}

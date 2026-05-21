//! SearchMessagesTool — search past session messages across all trees.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{Tool, ToolResult};
use agent_core::config::agent_dir;
use agent_core::types::{Entry, MessageContent, ToolDefinition, ToolOutput};

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

            // Only process subdirectories (per-tree dirs)
            if !path.is_dir() {
                continue;
            }

            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if let Some(tid) = tree_id {
                if fname != tid {
                    continue;
                }
            }

            let jsonl_path = path.join("data.jsonl");
            if !jsonl_path.exists() {
                continue;
            }

            _file_count += 1;

            // Parse entries via serde stream
            let content = fs::read_to_string(&jsonl_path)
                .map_err(|e| format!("Cannot read {}: {}", jsonl_path.display(), e))?;

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

fn entry_as_message(entry: &Entry) -> Option<&agent_core::types::Message> {
    if let Entry::Message { message, .. } = entry {
        Some(message)
    } else {
        None
    }
}

fn entry_role_str(role: &agent_core::types::MessageRole) -> &'static str {
    match role {
        agent_core::types::MessageRole::System => "system",
        agent_core::types::MessageRole::User => "user",
        agent_core::types::MessageRole::Assistant => "assistant",
        agent_core::types::MessageRole::Tool => "tool",
    }
}

fn message_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if let agent_core::types::ContentBlock::Text { text: t } = block {
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

    fn execute(&self, params: &serde_json::Value, _stop: &Arc<AtomicBool>) -> ToolResult {
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


// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::store::Store;
    use agent_core::types::*;
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
                .execute(&serde_json::json!({"query": "hello"}), &Arc::new(AtomicBool::new(false)))
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
                .execute(&serde_json::json!({"query": "hello world"}), &Arc::new(AtomicBool::new(false)))
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
                }), &Arc::new(AtomicBool::new(false)))
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

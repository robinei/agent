use std::path::PathBuf;
use serde::{Deserialize, Serialize};

// ── Identifiers ──

pub type TreeId = String;
pub type EntryId = String; // 8-char hex

// ── Tree sandbox config ──

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TreeSandbox {
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    #[serde(default)]
    pub network: Option<bool>,
    #[serde(default)]
    pub hide: Vec<PathBuf>,
    #[serde(default)]
    pub unhide: Vec<PathBuf>,
}

// ── Tree metadata ──

/// Tree metadata. The server is the sole writer of meta.json. Workers
/// communicate desired meta changes via events; the server applies them.
/// This invariant means a worker cannot redirect a future spawn by
/// rewriting its own meta to point at a different repo_path or escalated
/// sandbox config.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TreeMeta {
    pub id: TreeId,
    pub parent_id: Option<TreeId>,
    /// Repo directory, canonicalized at tree creation and immutable afterward.
    pub repo_path: Option<std::path::PathBuf>,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Points to the most recent entry. None = empty tree.
    pub leaf_id: Option<EntryId>,
    #[serde(default)]
    pub sandbox: TreeSandbox,
}

/// Validate and canonicalize a repo_path for tree creation.
/// Returns the canonical path on success, or an error string explaining why
/// the path was rejected.
pub fn validate_repo_path(
    repo_path: &std::path::Path,
    defaults_hide: &[PathBuf],
    sandbox: &TreeSandbox,
) -> Result<PathBuf, String> {
    let canon = std::fs::canonicalize(repo_path)
        .map_err(|e| format!("canonicalize {:?}: {}", repo_path, e))?;
    if !canon.is_dir() {
        return Err(format!("{:?} is not a directory", canon));
    }

    let home = dirs_home();
    let banned: &[&std::path::Path] = &[
        std::path::Path::new("/"),
        &home,
        &home.join(".agent"),
        &home.join(".config/agent"),
    ];
    if banned.iter().any(|b| canon == **b) {
        return Err(format!("repo_path {:?} is not allowed", canon));
    }

    // Reject if canon == any default-hide path (after applying unhide)
    let effective_hide: Vec<PathBuf> = defaults_hide
        .iter()
        .chain(sandbox.hide.iter())
        .filter(|p| !sandbox.unhide.contains(p))
        .map(expand_tilde)
        .collect();
    if effective_hide.iter().any(|h| canon == *h || canon.starts_with(h)) {
        return Err(format!(
            "repo_path {:?} overlaps a hidden directory",
            canon
        ));
    }
    Ok(canon)
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

// ── LSP types ──

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LspConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub servers: Vec<LspServerConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LspServerConfig {
    pub language: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_lsp_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_lsp_silence")]
    pub silence_ms: u64,
}

fn default_lsp_timeout() -> u64 { 5000 }
fn default_lsp_silence() -> u64 { 500 }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Position { pub line: u32, pub character: u32 }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Range { pub start: Position, pub end: Position }

#[derive(Serialize, Deserialize, Clone, Debug, Copy, PartialEq)]
#[repr(u8)]
pub enum DiagnosticSeverity { Error = 1, Warning = 2, Information = 3, Hint = 4 }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
    pub code: Option<String>,
}

pub fn expand_tilde(p: &PathBuf) -> PathBuf {
    if p.starts_with("~") {
        let home = dirs_home();
        if p == &PathBuf::from("~") {
            home
        } else {
            let stripped = p.strip_prefix("~").unwrap();
            home.join(stripped)
        }
    } else {
        p.clone()
    }
}

/// Derive the current goal from the entry tree by walking from leaf_id to root.
pub fn current_goal(entries: &[Entry], leaf_id: &str) -> Option<String> {
    let map: std::collections::HashMap<&str, &Entry> =
        entries.iter().map(|e| (e.id(), e)).collect();
    let mut current = Some(leaf_id);
    while let Some(cid) = current {
        if let Some(Entry::GoalSet { goal, .. }) = map.get(cid).copied() {
            return Some(goal.clone());
        }
        current = map.get(cid).and_then(|e| e.parent_id());
    }
    None
}

// ── File header (first line of .jsonl) ──

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TreeHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u8,
    pub id: TreeId,
    pub total_tokens: u64,
    pub current_model: String,
}

// ── Tree entries ──
//
// Uses `entry_type` (not `type`) as the tag to avoid a collision when
// ServerEvent::Entry(Entry) is serialized — ServerEvent uses `type` as its
// tag, and having both enums use the same tag name would produce duplicate
// JSON keys ({"type":"entry","type":"message",...}).

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "entry_type")]
pub enum Entry {
    #[serde(rename = "session_start")]
    SessionStart {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
    },
    #[serde(rename = "message")]
    Message {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        message: Message,
    },
    #[serde(rename = "session_end")]
    SessionEnd {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        summary: Option<String>,
        status: SessionStatus,
        #[serde(rename = "continuationBrief")]
        continuation_brief: Option<String>,
    },
    #[serde(rename = "bash_exec")]
    BashExec {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        command: String,
        output: String,
        #[serde(rename = "exitCode")]
        exit_code: i32,
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    #[serde(rename = "model_set")]
    ModelSet {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        model: String,
    },
    #[serde(rename = "label")]
    Label {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        label: String,
    },
    #[serde(rename = "goal_set")]
    GoalSet {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        goal: String,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::SessionStart { id, .. }
            | Entry::Message { id, .. }
            | Entry::SessionEnd { id, .. }
            | Entry::BashExec { id, .. }
            | Entry::ModelSet { id, .. }
            | Entry::Label { id, .. }
            | Entry::GoalSet { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::SessionStart { parent_id, .. }
            | Entry::Message { parent_id, .. }
            | Entry::SessionEnd { parent_id, .. }
            | Entry::BashExec { parent_id, .. }
            | Entry::ModelSet { parent_id, .. }
            | Entry::Label { parent_id, .. }
            | Entry::GoalSet { parent_id, .. } => parent_id.as_deref(),
        }
    }
}

// ── Messages ──

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Text(String::new())
    }
}

impl MessageContent {
    /// Deserialize from raw JSON, handling `null`, empty, or string/array content.
    pub fn from_json_value(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => MessageContent::Text(String::new()),
            serde_json::Value::String(s) => MessageContent::Text(s.clone()),
            serde_json::Value::Array(_) => {
                serde_json::from_value::<Vec<ContentBlock>>(value.clone())
                    .map(MessageContent::Blocks)
                    .unwrap_or_else(|_| MessageContent::Text(String::new()))
            }
            _ => MessageContent::Text(String::new()),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

// ── Supporting types ──

/// A completed tool call (accumulated from streaming deltas).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Partially-built tool call during streaming (accumulates argument text).
#[derive(Default, Clone, Debug)]
pub struct ToolCallBuilder {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum StopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "tool_calls")]
    ToolCalls,
    #[serde(rename = "content_filter")]
    ContentFilter,
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SessionStatus {
    #[serde(rename = "continuing")]
    Continuing,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "aborted")]
    Aborted,
    #[serde(rename = "blocked")]
    Blocked,
}

// ── LLM streaming response types ──

/// Provider-neutral chat chunk. Server parses each provider's wire format
/// into this shape before sending it across the pipe to the worker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatChunk {
    pub delta_text: Option<String>,
    pub delta_reasoning: Option<String>,
    #[serde(default)]
    pub tool_call_delta: Vec<DeltaToolCall>,
    pub finish_reason: Option<StopReason>,
    pub usage: Option<TokenUsage>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeltaToolCall {
    pub index: Option<u32>,
    pub id: Option<String>,
    #[serde(default)]
    pub function: DeltaToolCallFunction,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct DeltaToolCallFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

// ── Server events (sent from agent thread to CLI/PWA over SSE) ──

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
    Fatal,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ServerEvent {
    /// Token delta during streaming (never stored).
    #[serde(rename = "text_chunk")]
    TextChunk { content: String },
    /// Before tool executes — live progress.
    #[serde(rename = "tool_start")]
    ToolStart {
        tool: String,
        input: serde_json::Value,
    },
    /// After tool finishes — shows output.
    #[serde(rename = "tool_result")]
    ToolResult {
        tool: String,
        exit: i32,
        output: String,
    },
    /// Any persisted entry was just written to the tree.
    #[serde(rename = "entry")]
    Entry(Entry),
    /// Runtime advisory only (never stored).
    #[serde(rename = "cap_warning")]
    CapWarning { level: String, pct: u8 },
    /// User-visible notification from the worker (info / warning / error / fatal).
    #[serde(rename = "notification")]
    Notification { level: NotificationLevel, message: String },
    /// Turn complete — CLI shows prompt.
    #[serde(rename = "done")]
    Done { status: String },
    /// External file change — PWA file view.
    #[serde(rename = "file_changed")]
    FileChanged { path: String, kind: String },
    /// Meta field update (e.g., auto-title result). Broadcast by server;
    /// never persisted in the event stream.
    #[serde(rename = "meta_update")]
    MetaUpdate { title: Option<String> },
    /// Reasoning/thinking token from models that expose extended thinking.
    /// Never stored. Rendered in dimmed color during streaming.
    #[serde(rename = "thinking_chunk")]
    ThinkingChunk { content: String },
}

// ── Tool system types ──

/// Definition of a tool, sent to the LLM as a JSON Schema function definition.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Output from executing a tool.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    pub content: String,
    pub truncated: bool,
    pub original_size: usize,
    pub exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_meta_sandbox_default() {
        let json = r#"{"id":"abc","parent_id":null,"repo_path":null,"title":"test","created_at":100,"updated_at":100,"leaf_id":null}"#;
        let meta: TreeMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.sandbox, TreeSandbox::default());
    }

    #[test]
    fn test_validate_repo_path_rejects_home() {
        let home = dirs_home();
        let result = validate_repo_path(&home, &[], &TreeSandbox::default());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed"));
    }

    #[test]
    fn test_validate_repo_path_rejects_root() {
        let result = validate_repo_path(std::path::Path::new("/"), &[], &TreeSandbox::default());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed"));
    }

    #[test]
    fn test_validate_repo_path_rejects_hidden_overlap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hidden_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let sub_path = hidden_dir.join("something");
        std::fs::create_dir_all(&sub_path).unwrap();
        let result = validate_repo_path(
            &sub_path,
            &[hidden_dir],
            &TreeSandbox::default(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("hidden"));
    }

    #[test]
    fn test_validate_repo_path_accepts_normal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("Code").join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let result = validate_repo_path(&repo, &[], &TreeSandbox::default());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), std::fs::canonicalize(&repo).unwrap());
    }

    #[test]
    fn test_expand_tilde() {
        let home = dirs_home();
        assert_eq!(expand_tilde(&PathBuf::from("~")), home);
        assert_eq!(expand_tilde(&PathBuf::from("~/foo")), home.join("foo"));
        assert_eq!(expand_tilde(&PathBuf::from("/abs/path")), PathBuf::from("/abs/path"));
    }
}

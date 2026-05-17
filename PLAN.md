# Agent Server — Plan

## Goal

Personal coding agent server running on a home PC, accessible via Tailscale.
Work on hobby repos from an iPhone (or other mobile device) by directing the agent
and watching it work.

The CLI is the primary development and testing ground, but the real target is a
PWA (served from the same server process) that works well on mobile browsers over
Tailscale. The server-first architecture and SSE streaming are designed for this:
the CLI exercises the same API surface the PWA will use, so we validate the
protocol and event model long before writing any frontend code. Voice input
(via Web Speech API on iOS Safari) is a stretch goal.

I don't want regular session compaction. Instead we have a soft cap at which point the agent and/or user
is informed that context limit is approaching so either wrap up or prepare for handover.
If that does not occur by the time the hard cap is reached the server-agent forces end and generates
the continuation brief. That is then injected as a `session_end` marker in the tree, from which
context building will use the brief instead of older messages.

---

## Principles

- Synchronous threaded Rust (no async runtime)
- Minimal, well-chosen dependencies
- Start small, expand later
- CLI client first, PWA later
- Extensible from the start (hooks system for tools, events, and context)

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  agent-server (binary)                                                │
│                                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                          │
│  │ rouille   │  │ Agent    │  │ Agent    │                          │
│  │ HTTP      │  │ Thread   │  │ Thread   │                          │
│  │ Handler   │  │ (tree A) │  │ (tree B) │                          │
│  │ (thread   │  │          │  │          │                          │
│  │  pool)    │◄►│ mpsc     │  │ mpsc     │                          │
│  └────┬──────┘  └────┬─────┘  └────┬─────┘                          │
│       │              │             │                                 │
│       └──────────────┴─────────────┘                                 │
│                      │                                               │
│              ┌───────┴──────────────────────┐                        │
│              │  agent-core (lib)            │                        │
│              │  types | store | provider    │                        │
│              │  tools | agent | search      │                        │
│              └──────────────────────────────┘                        │
│                      │                                               │
│              ┌───────┴────────┐                                      │
│              │  JSONL Files   │                                      │
│              │  (~/.agent/)   │                                      │
│              └────────────────┘                                      │
└──────────────────────────────────────────────────────────────────────┘
         ▲ HTTP + SSE                     ▲ HTTP + SSE
         │                                │
┌────────┴────────┐              ┌────────┴────────┐
│  agent-cli      │              │  PWA Client     │
│  (binary)       │              │  (future)       │
│  ureq HTTP      │              └─────────────────┘
│  termion TUI    │
└─────────────────┘
```

The server runs the agent loops. The CLI is a thin display client that connects
over HTTP — no agent logic. Both are separate binaries sharing `agent-core` as
a library for types and (for the CLI) HTTP request/response models.

Multiple agent threads run concurrently (one per active tree). Each has a
channel-based mailbox (`std::sync::mpsc`).

---

## Naming

| Internal | User-facing |
|---|---|
| tree | tree |
| session | (internal — segment between `session_start` and `session_end`) |

---

## Stack

| Concern | Choice |
|---|---|
| HTTP server | `rouille` (thread pool, no async) |
| HTTP client | `ureq` (sync, no async) |
| Storage | JSONL files under `~/.agent/` |
| JSON | `serde` + `serde_json` |
| CLI args | `clap` |
| CLI colors | `termion` (ANSI codes, no TUI framework) |
| LLM | OpenAI-compatible chat completions (llama.cpp, OpenRouter) |
| Auth | None — Tailscale handles network access |

---

## Dependencies

### Crate: `agent-core` (library)

```toml
[package]
name = "agent-core"
version = "0.1.0"
edition = "2021"

[dependencies]
serde                   = { version = "1", features = ["derive"] }
serde_json              = "1"
uuid                    = { version = "1", features = ["v4"] }
chrono                  = { version = "0.4", features = ["serde"] }
ureq                    = "3"        # LLM API calls (sync, no tokio)
unicode-normalization   = "0.1"      # NFKC normalization for edit fuzzy matching
regex                   = "1"        # grep tool, search
walkdir                 = "2"        # find/glob tool, context files discovery
notify                  = "8"        # filesystem watcher (PWA file view, agent file change info)
log                     = "0.4"      # logging facade
env_logger              = "0.11"     # stderr logger (RUST_LOG control)
thiserror               = "1"        # derive(Error) for library error types
nix                     = { version = "0.29", features = ["signal", "process"] }  # process group kill for bash tool

[dev-dependencies]
tempfile = "3"       # temp dirs in tool tests
```

Shared library: types (`Entry`, `Message`, etc.), store (JSONL I/O, file locking),
provider (LLM request builder + response parser), tools (implementations + definitions),
agent loop, search. No server or CLI code.

### Crate: `agent-server` (binary)

```toml
[package]
name = "agent-server"
version = "0.1.0"
edition = "2021"

[dependencies]
agent-core    = { path = "../agent-core" }
rouille       = "3"       # HTTP server, routing, SSE streaming
serde         = { version = "1", features = ["derive"] }
serde_json    = "1"
anyhow        = "1"        # error handling in route handlers
```

Minimal — just HTTP wiring on top of the core library.

### Crate: `agent-cli` (binary)

```toml
[package]
name = "agent-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
agent-core    = { path = "../agent-core" }
ureq          = "3"       # HTTP client for server API
serde         = { version = "1", features = ["derive"] }
serde_json    = "1"
clap          = { version = "4", features = ["derive"] }
termion       = "4"       # ANSI colors, cursor hide/show
anyhow        = "1"        # error handling in CLI client
```

---

## Project Structure

```
agent/
  agent-core/
    Cargo.toml
    src/
      lib.rs              -- Re-exports
      types.rs            -- All data types, serde derives, Entry enum, SSE events
      store.rs            -- JSONL read/write, tree metadata, file locking
      provider.rs         -- LLM provider (request builder, response parser)
      config.rs           -- Config loading (~/.agent/config.toml, env vars)
      logging.rs          -- Simple file + stderr logging
      hooks.rs            -- Hook trait + registry (on_tool_call, on_before_context, etc.)
      tools/
        mod.rs            -- Tool trait + all_tools() registry
        read.rs           -- Read tool
        write.rs          -- Write tool
        edit.rs           -- Edit tool (exact match first, then fuzzy fallback)
        bash.rs           -- Bash tool
        grep.rs           -- Grep tool (regex search across files)
        find.rs           -- Find tool (walkdir glob/pattern search)
        ls.rs             -- Ls tool (directory listing)
        git.rs            -- Git tool (status, diff, log, structured results)
        search.rs         -- Session/tree search (JSONL stream deserialization)
      file_watcher.rs     -- Filesystem watcher (notify crate), watches repo directory
      context_files.rs    -- AGENTS.md/CLAUDE.md discovery, skills loading
      agent.rs            -- Agent loop: build context → call LLM → dispatch tools → repeat

  agent-server/
    Cargo.toml
    src/
      main.rs             -- Entry point: rouille server, route registration
      routes.rs           -- Route handlers: /trees, /trees/{id}/message, /trees/{id}/stream, etc.
      lifecycle.rs        -- Agent thread spawn/kill, handle registry, mailbox wiring

  agent-cli/
    Cargo.toml
    src/
      main.rs             -- Entry point: clap subcommands
      interactive.rs      -- TUI loop: tree selection, prompt, SSE display
      client.rs           -- HTTP client helpers: list trees, send message, read SSE stream

### Workspace Root (`agent/Cargo.toml`)

```toml
[workspace]
resolver = "2"
members = [
    "agent-core",
    "agent-server",
    "agent-cli",
]
```

---

## Data Types (`agent-core/src/types.rs`)

```rust
// ── Identifiers ──
pub type TreeId = String;
pub type EntryId = String;  // 8-char hex

// ── Tree metadata ──
#[derive(Serialize, Deserialize)]
pub struct TreeMeta {
    pub id: TreeId,
    pub parent_id: Option<TreeId>,
    /// Repo directory, canonicalized at tree creation and immutable afterward.
    /// Set once via POST /trees; never mutated.
    pub repo_path: Option<std::path::PathBuf>,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Points to the most recent entry. None = empty tree.
    /// Status is implicit: points to a session_end → ended;
    /// otherwise → active.
    pub leaf_id: Option<EntryId>,
}

/// Derive the current goal from the entry tree by walking from leaf_id to root
/// and returning the text of the most recent `GoalSet` entry found.
pub fn current_goal(entries: &[Entry], leaf_id: &str) -> Option<String> {
    // Walk parent_id chain from leaf_id to root, return first GoalSet.goal found
}

// ── File header (first line of .jsonl) ──
#[derive(Serialize, Deserialize)]
pub struct TreeHeader {
    #[serde(rename = "type")]
    pub kind: String,           // "meta"
    pub version: u8,
    pub id: TreeId,
    pub total_tokens: u64,
    /// Cache of the current model. Updated whenever a ModelSet entry is appended.
    /// Source of truth is the most recent ModelSet entry in the tree.
    pub current_model: String,
}

// ── Tree entries ──
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
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
        #[serde(rename = "status")]
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
    Label { /* ... */ },
    #[serde(rename = "goal_set")]
    GoalSet {
        id: EntryId,
        #[serde(rename = "parentId")]
        parent_id: Option<EntryId>,
        timestamp: String,
        goal: String,
    },
}

// ── Messages ──
#[derive(Serialize, Deserialize)]
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

pub enum MessageRole { System, User, Assistant, Tool }

#[derive(Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum MessageContent {
    #[default]
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    /// Deserialize from raw JSON, handling `null`, empty, or string/array content.
    /// Falls back to empty string gracefully.
    pub fn from_json_value(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => MessageContent::Text(String::new()),
            serde_json::Value::String(s) => MessageContent::Text(s.clone()),
            serde_json::Value::Array(arr) => {
                serde_json::from_value::<Vec<ContentBlock>>(value.clone())
                    .map(MessageContent::Blocks)
                    .unwrap_or(MessageContent::Text(String::new()))
            }
            _ => MessageContent::Text(String::new()),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "toolCall")]
    ToolCall { id: String, name: String, arguments: serde_json::Value },
}

// ── Supporting types ──

/// A completed tool call (accumulated from streaming deltas).
#[derive(Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Partially-built tool call during streaming (accumulates argument text).
#[derive(Default)]
pub struct ToolCallBuilder {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Stop reason from the LLM.
#[derive(Serialize, Deserialize, Clone)]
pub enum StopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "tool_calls")]
    ToolCalls,
    #[serde(rename = "content_filter")]
    ContentFilter,
}

/// Token usage returned by the provider.
#[derive(Serialize, Deserialize, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Status of a completed session segment.
#[derive(Serialize, Deserialize, Clone)]
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

/// Input sent to an agent thread from the server.
pub enum AgentInput {
    /// New user message for the agent to process.
    Message { text: String },
    /// Signal the agent to stop after the current tool call.
    Stop,
}

/// A single chunk from the LLM streaming response.
#[derive(Deserialize)]
pub struct ChatChunk {
    pub choices: Vec<Choice>,
    pub usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
pub struct Choice {
    pub delta: Delta,
    pub finish_reason: Option<String>,
    pub index: u32,
}

#[derive(Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<DeltaToolCall>,
}

#[derive(Deserialize)]
pub struct DeltaToolCall {
    pub index: Option<u32>,
    pub id: Option<String>,
    #[serde(default)]
    pub function: DeltaToolCallFunction,
}

#[derive(Deserialize, Default)]
pub struct DeltaToolCallFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Iterator over SSE lines from the LLM streaming response.
///
/// NOTE on lifetimes: `ureq::Response::into_reader()` consumes the response and
/// returns `BodyReader<'static>`. We do NOT need to keep the original `Response`
/// alive — the reader owns all the state internally. This is the version used in
/// implementation.
pub struct ChatStream {
    reader: std::io::BufReader<ureq::BodyReader<'static>>,
}

impl ChatStream {
    pub fn new(response: ureq::Response) -> Self {
        Self { reader: std::io::BufReader::new(response.into_reader()) }
    }

    /// Read the next SSE line. Returns `None` on EOF or error.
    pub fn next_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

// Note: ChatStream does NOT implement Iterator because next_line() borrows self
// mutably (reads from the underlying BufReader). If you need an iterator wrapper,
// use: `std::iter::from_fn(|| stream.next_line())` which works around the
// borrow-checker limitation.
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

// ── Server events (sent from agent thread to CLI/PWA over SSE) ──
//
// Split into two categories:
//   Transient — live progress, never persisted (TextChunk, ToolStart, etc.)
//   Entry    — whatever just got written to the JSONL tree
//
// The CLI deduplicates: if a Transient event already rendered the data
// (e.g., ToolStart + ToolResult → render live), it skips the subsequent
// Entry(BashExec) by matching EntryId.
//
// Streaming flow:
//   1. Server emits one TextChunk per LLM response delta (token/word)
//   2. CLI appends each TextChunk to current line — no newline
//   3. On tool call: ToolStart → ToolResult (synchronous) → resume TextChunks
//   4. On turn complete: Done { status }
//   5. Final full text is NOT re-sent — CLI already has it from TextChunks
//      (the Entry(Message) that arrives later is skipped by dedup)
#[derive(Serialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    /// Token delta during streaming (never stored)
    #[serde(rename = "text_chunk")]
    TextChunk { content: String },
    /// Before tool executes — live progress
    #[serde(rename = "tool_start")]
    ToolStart { tool: String, input: serde_json::Value },
    /// After tool finishes — shows output
    #[serde(rename = "tool_result")]
    ToolResult { tool: String, exit: i32, output: String },
    /// Any persisted entry was just written to the tree
    /// (Message, BashExec, GoalSet, ModelSet, SessionEnd, ...)
    /// The Entry enum at the storage layer IS the wire format.
    #[serde(rename = "entry")]
    Entry(Entry),
    /// Runtime advisory only (never stored)
    #[serde(rename = "cap_warning")]
    CapWarning { level: String, pct: u8 },
    /// Recoverable or fatal error
    #[serde(rename = "error")]
    Error { message: String, fatal: bool },
    /// Turn complete — CLI shows prompt
    #[serde(rename = "done")]
    Done { status: String },
    /// External file change — PWA file view
    #[serde(rename = "file_changed")]
    FileChanged { path: String, kind: String },
}
```

---

## LLM Provider (`agent-core/src/provider.rs`)

```rust
pub struct Provider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Provider {
    /// POST /v1/chat/completions with streaming.
    /// Returns an iterator over lines of the SSE response.
    pub fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatStream> { /* ureq POST, read body line by line */ }
}
```

Streaming requires `stream_options: {"include_usage": true}` for llama.cpp to
return token usage in the final chunk.

### Provider Guidance

- **llama.cpp**: Local inference, no API fees. Good for dev and small models (7B–34B).
- **OpenRouter**: Access to frontier models (Claude, GPT-4o, DeepSeek). Pay-per-token.
- Switch by changing config; no code changes needed.

---

## Tools (`agent-core/src/tools/`)

Each tool lives in its own file and implements a shared `Tool` trait.
The `mod.rs` collects them into a single registry.

### Tool Trait

```rust
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,  // JSON Schema object
}

pub struct ToolOutput {
    pub content: String,
    pub truncated: bool,       // true if output exceeded max_lines/max_bytes
    pub original_size: usize,  // pre-truncation size
    pub exit_code: Option<i32>,
}

pub trait Tool: Send {
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, params: &serde_json::Value) -> Result<ToolOutput>;
}
```

### Registry

```rust
pub fn all_tools(cwd: &Path) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool::new(cwd)),
        Box::new(WriteTool::new(cwd)),
        Box::new(EditTool::new(cwd)),
        Box::new(BashTool::new(cwd)),
        Box::new(GrepTool::new(cwd)),
        Box::new(FindTool::new(cwd)),
        Box::new(LsTool::new(cwd)),
        Box::new(GitTool::new(cwd)),
    ]
}
```

To add a new tool: create `tools/my_tool.rs`, implement `Tool`, add one line
in `all_tools()`. No match arms or enums to update.

### Testability

Tools use real filesystem calls directly (no generic I/O trait parameters).
Tests create temp directories with fixture files and run tools against those.
This is simpler and catches real I/O bugs that mocks miss.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;  // dev dependency

    #[test]
    fn test_read_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let tool = ReadTool::new(dir.path());
        let result = tool.execute(&serde_json::json!({ "path": "test.txt" })).unwrap();
        assert_eq!(result.content, "hello\n");
    }
}
```

### Per-Tool Output Limits

Every tool enforces a hard cap on output size before returning, preventing
runaway responses that flood context:

| Tool   | Default Limit               |
|--------|-----------------------------|
| `read` | 2000 lines / 50 KB          |
| `bash` | 2000 lines / 50 KB          |
| `ls`   | 500 entries                 |
| `grep` | 100 matches, 15 lines each  |
| `edit` | (none — returns diff only)  |
| `write`| (none — returns ack only)   |

Each tool reports `truncated: true` when the limit was hit, so the agent
loop can tell the LLM: "Output truncated after N lines. Use a more specific
query to narrow results."

### Edit Tool

Edit uses exact-match first, then fuzzy fallback:

1. Strip BOM from file
2. Normalize line endings to LF
3. Exact `str::find` — success? Apply, done.
4. Fuzzy pass: NFKC normalize, strip trailing whitespace per line,
   normalize smart quotes → ASCII, dashes → `-`, special spaces → ` `
5. `str::find` in normalized space
6. Verify uniqueness, build full diff, apply replacement

Edits in one call are matched against the same original content, then
applied in reverse order so offsets remain stable. Overlapping edits
error out.

### File Mutation Queue

Concurrent writes to the same file (from any tool) are serialized per-file.
Before any mutation, resolve the path through `canonicalize()` and acquire
a per-file `Mutex`:

```rust
use std::sync::LazyLock;

static FILE_MUTEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_file_lock<F, T>(path: &Path, f: F) -> T
where F: FnOnce() -> T {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut locks = FILE_MUTEXES.lock().unwrap();
    let lock = locks.entry(canon).or_default().clone();
    drop(locks);
    let _guard = lock.lock().unwrap();
    f()
}
```

Both `edit` and `write` use `with_file_lock`. `read` does not — concurrent
reads are safe. Different files run in parallel.

### Bash Tool

Bash uses `std::process::Command` with process group management to
ensure descendant processes can be killed on timeout:

```rust
use std::os::unix::process::CommandExt;

let mut cmd = Command::new("bash");
cmd.arg("-c").arg(&command);
// Create new process group so we can kill all descendants
unsafe { cmd.process_group(0); }
let mut child = cmd.spawn()?;
// ... wait with timeout ...
// Kill entire process group
nix::sys::signal::killpg(
    nix::unistd::Pid::from_raw(child.id() as i32),
    nix::sys::signal::Signal::SIGTERM,
).ok();
```

### Git Tool

Wraps `git` subprocess but returns structured JSON output instead of raw
terminal text. The agent gets structured data (changed files, diff stats,
branch info) without having to parse human-oriented output.

```rust
pub struct GitOutput {
    pub changed_files: Vec<GitFileChange>,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub stderr: String,
    pub exit_code: i32,
}

pub struct GitFileChange {
    pub path: String,
    pub status: String,  // M, A, D, R, ??
    pub staged: bool,
}
```

Subcommands available to the agent: `status`, `diff`, `log`, `show`, `add`, `commit`, `push`, `pull`.
No python tool — bash handles python invocations.

### Grep Tool

Recursive file content search using `regex` crate. Skips binary files and
`.git/`, `node_modules/`, `target/` by default.

```rust
pub fn execute(&self, params: &serde_json::Value) -> Result<ToolOutput> {
    // pattern: regex string
    // path: optional subdirectory or file glob
    // max_matches: default 100
    // context_lines: default 0
}
```

### Find Tool

File/directory search using `walkdir`.

```rust
pub fn execute(&self, params: &serde_json::Value) -> Result<ToolOutput> {
    // pattern: glob or substring (e.g., "*.rs", "*test*")
    // path: optional subdirectory
    // type: "file" | "dir" | "both"
    // max_results: default 500
}
```

### Ls Tool

Directory listing with file type and size info.

```rust
pub fn execute(&self, params: &serde_json::Value) -> Result<ToolOutput> {
    // path: directory to list (default cwd)
    // recursive: bool (default false)
    // max_entries: default 500
}
```

### Search Tools (`agent-core/src/tools/search.rs`)

Two additional tools for searching past sessions. Scan JSONL files using
`serde_json::StreamDeserializer` to avoid loading entire files into memory:

```rust
pub fn search_messages(
    query: &str, tree_id: Option<&str>, limit: usize
) -> Result<Vec<SearchResult>>;

pub fn search_files(
    path: &str, limit: usize
) -> Result<Vec<FileHit>>;
```

Both are registered in `all_tools()` alongside file tools.

### Tool — LLM Definitions

```rust
pub fn all_tool_definitions(cwd: &Path) -> Vec<ToolDefinition> {
    all_tools(cwd).iter().map(|t| t.definition()).collect()
}
```

## Hooks & Extensibility

A simple hook system allows the server to intercept events without forking
the core. Hooks are registered at startup via config or programmatically.

### Hook Trait

```rust
pub enum HookAction {
    /// Let the event proceed normally
    PassThrough,
    /// Block the event (return error, prevent tool execution, etc.)
    Block { reason: String },
    /// Replace the event payload
    Replace { payload: serde_json::Value },
    /// Log / observe only
    Observe,
}

pub trait Hook: Send {
    fn name(&self) -> &'static str;

    /// Called before a tool executes. Return Block to prevent execution.
    fn on_tool_call(&self, _tool: &str, _params: &serde_json::Value) -> Result<HookAction> {
        Ok(HookAction::PassThrough)
    }

    /// Called after context is built but before LLM call.
    fn on_before_llm_call(&self, _messages: &mut Vec<Message>) -> Result<HookAction> {
        Ok(HookAction::PassThrough)
    }

    /// Called after session_end is generated.
    fn on_session_end(&self, _summary: &str) -> Result<HookAction> {
        Ok(HookAction::PassThrough)
    }

    /// Called on server startup.
    fn on_startup(&self) -> Result<()> {
        Ok(())
    }
}
```

### Hook Registry

```rust
static HOOKS: LazyLock<Mutex<Vec<Box<dyn Hook>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register_hook(hook: Box<dyn Hook>) { /* push to HOOKS */ }

pub fn run_tool_call_hooks(tool: &str, params: &serde_json::Value) -> Result<()> {
    for hook in HOOKS.lock().unwrap().iter() {
        match hook.on_tool_call(tool, params)? {
            HookAction::Block { reason } => bail!(reason),
            HookAction::Replace { payload } => { /* update params */ },
            HookAction::PassThrough | HookAction::Observe => {},
        }
    }
    Ok(())
}

pub fn run_before_llm_hooks(messages: &mut Vec<Message>) -> Result<()> {
    for hook in HOOKS.lock().unwrap().iter() {
        match hook.on_before_llm_call(messages)? {
            HookAction::Block { reason } => bail!(reason),
            HookAction::Replace { payload } => { /* deserialize into messages */ },
            HookAction::PassThrough | HookAction::Observe => {},
        }
    }
    Ok(())
}
```

### Use Cases

| Hook | What it enables |
|------|----------------|
| `on_tool_call` | Permission gates (confirm before `rm -rf`, `sudo`), path protection (block writes to `.env`), secret redaction |
| `on_before_llm_call` | Inject system messages, prune stale context, rewrite prompts |
| `on_session_end` | Post-summarization analysis, notify external services |
| `on_startup` | Health checks, report server URI to user |

Hooks are loaded from `~/.agent/hooks/` (`.rs` files compiled and loaded via
dlopen/dynamic lib, or more practically: a `~/.agent/config.toml` field
listing pre-built `.so` files). For personal use, simply adding code to
`agent-core/src/hooks.rs` at compile time is also acceptable — the trait
ensures consistent interfaces.

---

## Data Model (`agent-core/src/store.rs`)

All data stored as JSONL files under `~/.agent/`.

### File Layout

```
~/.agent/
  index.json                  -- Vec<TreeMeta> index (rebuilt from scanning trees/ on corruption)
  trees/
    <tree_uuid>.jsonl         -- Line 1: TreeHeader, Lines 2+: Entry
    <tree_uuid>.meta.json     -- Per-tree metadata (TreeMeta, single JSON object)
```

### Per-Tree Metadata

Each tree gets its own `{uuid}.meta.json` file. The `index.json` is a
summary cache rebuilt by scanning `trees/*.meta.json` on startup.

Benefits over a single `trees.json`:
- Write one file per mutation instead of rewriting the entire index
- Corruption in one tree's metadata doesn't affect others
- Index can be rebuilt from the source of truth (`.meta.json` files)
- Parallel reads/writes to different trees' metadata don't contend

```rust
/// Resolve the filesystem path for a tree's metadata file.
fn tree_dir() -> PathBuf {
    agent_dir().join("trees")
}

fn meta_path(id: &str) -> PathBuf {
    tree_dir().join(format!("{}.meta.json", id))
}

pub fn load_tree_meta(id: &str) -> Result<Option<TreeMeta>> {
    let path = meta_path(id);
    if !path.exists() { return Ok(None); }
    let content = fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&content)?))
}
```    let content = fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&content)?))
}

/// In-memory index cache. Rebuilt from `.meta.json` files on startup.
/// Written to `index.json` only during graceful shutdown.
static INDEX_CACHE: LazyLock<Mutex<HashMap<TreeId, TreeMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn save_tree_meta(meta: &TreeMeta) -> Result<()> {
    let path = meta_path(&meta.id);
    // Atomic write: write to temp, rename over target
    let tmp = path.with_extension("meta.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(meta)?)?;
    fs::rename(&tmp, &path)?;
    // Update in-memory index cache (index.json is rebuilt on startup, not written synchronously)
    update_index_cache(meta);
    Ok(())
}

/// Update the in-memory index cache. The index.json file on disk is only
/// written during graceful shutdown or periodically; it is always rebuilt
/// from `.meta.json` files on startup, so it is safe for the disk copy to
/// be stale or absent.
pub fn update_index_cache(meta: &TreeMeta) {
    INDEX_CACHE.lock().unwrap().insert(meta.id.clone(), meta.clone());
}

pub fn rebuild_index() -> Result<Vec<TreeMeta>> {
    let mut trees = Vec::new();
    for entry in fs::read_dir(trees_dir())? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            continue; // skip .jsonl files
        }
        if path.extension().and_then(|e| e.to_str()) == Some("meta") {
            let content = fs::read_to_string(&path)?;
            trees.push(serde_json::from_str(&content)?);
        }
    }
    trees.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(trees)
}
```

### Tree File — Line 1 (Header)

```json
{"type":"meta","version":1,"id":"<uuid>","total_tokens":0,"current_model":"openrouter/..."}
```

`total_tokens` is cumulative for the current session segment. Compared against
soft/hard caps. Reset after `session_end`.

### Store API

```rust
pub fn list_trees() -> Result<Vec<TreeMeta>>;
pub fn get_tree(id: &str) -> Result<Option<TreeMeta>>;
pub fn update_tree(meta: &TreeMeta) -> Result<()>;

pub fn create_tree_file(id: &str, model: &str) -> Result<()>;
pub fn append_entry(tree_id: &str, entry: &Entry) -> Result<()>;
pub fn read_all_entries(tree_id: &str) -> Result<Vec<Entry>>;
pub fn update_header(tree_id: &str, updates: &serde_json::Value) -> Result<()>;
```

## Context Files (`agent-core/src/context_files.rs`)

Load project-specific instructions from `AGENTS.md` / `CLAUDE.md` files.
Walk up from `cwd` to root, collecting all matches. Also check
`~/.agent/AGENTS.md` for global instructions.

```rust
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile>;
```

Files are concatenated and injected into the system prompt under a
`## Project Context` heading. The closest file to cwd comes last
(highest precedence).

### Skills

Optional skill discovery follows the [Agent Skills](https://agentskills.io)
standard. Skills directories live in `~/.agent/skills/` and `.agent/skills/`.
Each skill is a subdirectory with a `SKILL.md` containing YAML frontmatter:

```markdown
---
name: react-testing
description: Use when writing React component tests
---
# Instructions
1. Use @testing-library/react
2. Use vitest
```

Skills can be invoked explicitly via `/skill:name` or auto-triggered by the
agent when the LLM detects a matching description.

### Concurrency

Per-file `Mutex<std::fs::File>` for atomic appends. Append requires opening,
seeking to end, writing, and `fsync`.

```rust
static FILE_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<std::fs::File>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
```

Tree metadata files are written atomically via `write()`+`rename()` pattern
(write to temp, rename over target). The index is rebuilt on startup if missing
or corrupt.

---

## Tree Structure & Context Building (`agent-core/src/agent.rs`)

### The Tree

Entries form a tree via `parent_id`. `leaf_id` in `trees.json` points to the
current position.

```
[meta header]

  │  session 1
  │
session_start ─── msg1 ─── msg2 ─── msg3 ─── msg4
                                            │
session_end (brief, status: "continuing")
  │
  │  session 2
  │
session_start ─── msg5 ─── msg6 ← leaf

Branch at msg2:
  msg2 ─── msg7 ─── msg8 ← alternate leaf
```

### Context Building

Walk from leaf upward. Stop at `session_end` — inject its `continuation_brief`
as a system message. No `session_end` on path? See everything unfiltered.

`bash_exec` entries are used for rendering but filtered out of LLM context
(their content is already reflected in subsequent tool result messages) —
see rationale below.

> **Rationale:** The OpenAI/Anthropic message format embeds tool results
> as `Tool` role messages following the assistant message that called the tool.
> These tool result messages already contain the bash output. The separate
> `bash_exec` entry is a rendering-side convenience for the CLI/PWA. Filtering
> it avoids duplicating large outputs in the LLM context. If a future session
> search tool needs to retrieve bash output independently, `bash_exec` entries
> serve as that source of truth.

```rust
pub fn build_context(entries: &[Entry], leaf_id: &str) -> Vec<Message> {
    // Build HashMap<&EntryId, &Entry>
    // Walk parent chain from leaf_id to root
    // On SessionEnd with continuation_brief → insert system message, break
    // On Message → insert message into context (at front)
    // On GoalSet → inject system message from current_goal()
    //    (current_goal() walks the tree from leaf_id and finds the nearest GoalSet)
    // On ModelSet → track as current model (injected as system message)
    // Skip session_start, label, bash_exec
    // Use current_goal(entries, leaf_id) to find the active goal without duplicating
    // the walk logic.
    // Return Vec<Message> for LLM API
}
```

### Token Estimation Heuristic

Before each LLM API call, estimate the context size using a simple heuristic.
This lets us warn the user proactively rather than waiting for the API to
return usage data (which only arrives after the call completes):

```rust
fn estimate_tokens(content: &str) -> usize {
    // Code-heavy contexts average ~3 chars/token (shorter tokens like `fn`, `let`, `if`).
    // English prose averages ~4. This splitter uses 3.5 as a middle ground,
    // slightly conservative for code (avoids underestimating and hitting cap mid-call).
    (content.len() * 2 + 7) / 7  // ceil(content.len() / 3.5)
}

fn estimate_context_tokens(messages: &[Message]) -> usize {
    let mut total = 0;
    for msg in messages {
        match &msg.content {
            MessageContent::Text(s) => total += estimate_tokens(s),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => total += estimate_tokens(text),
                        ContentBlock::ToolCall { arguments, .. } => {
                            total += estimate_tokens(&arguments.to_string());
                        }
                    }
                }
            }
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                total += estimate_tokens(&call.arguments.to_string());
            }
        }
    }
    total
}
```

Combined with actual `total_tokens` from the API response (which is more
authoritative), the agent has two signals:
- Before call: `estimate_context_tokens()` vs context window
- After call: `total_tokens` from provider vs soft/hard cap

If estimates approach the hard cap, trigger the wrap-up sequence preemptively
instead of risking an overflow during the LLM call.

### Continuation Brief Generation

When the hard cap is reached, the server does NOT make the agent spend its
own context budget writing a summary. Instead:

1. **Hard cap hits** → agent loop is interrupted cleanly after the current
   tool call finishes (or immediately if idle).
2. **Server saves** what was accomplished (last assistant message, any pending
tool results) as a rough draft.
3. **Server makes a separate `session_end` summarization call** to the LLM
   with **only the messages from the just-finished session** as input
   (not the full agent context with tool outputs). This is typically
   many fewer tokens than the full context, leaving plenty of room in
   the summarization call's own context window for generating the brief.
4. The summarization result is written as the `continuation_brief` on the
   `session_end` entry.

Key insight: context windows limit **input + output** tokens per request.
The ~20% gap (65% soft → 85% hard) is for the agent's **real output**
(continuing to code, debug, etc.), not for meta-work. The summarization
call is a completely separate LLM request with a much smaller input
(just the session's messages, not the full tree context). It has its own
context budget independent of the agent's session.

This means:
- The agent uses 100% of its budget on actual coding
- The brief is generated post-hoc without pressure
- If the brief generation call fails, the session still ends cleanly
  (the server writes a minimal placeholder brief)
- A cheap/fast model can be used for summarization even if the agent
  uses a frontier model

```rust
fn generate_continuation_brief(
    provider: &Provider,
    messages: &[Message],  // only the session's messages, not full context
) -> Result<(String, SessionStatus)> {
    let summary_prompt = "Summarize what was accomplished in this coding
        session...";
    // ... separate LLM call with small context ...
}
```

### Continuation Brief Structure

Two parts concatenated:

**Part 1 — LLM-written:**
- What was accomplished
- Current file state
- Decisions made
- Unresolved issues
- Next steps

**Part 2 — Last 5 messages appended by server, marked with header:**
```
--- Last 5 messages from previous session (for context) ---
```

### Agent Loop

```rust
pub fn run_agent(
    tree_id: &str,
    input_rx: mpsc::Receiver<AgentInput>,
    output_tx: mpsc::Sender<ServerEvent>,
    stop: Arc<AtomicBool>,
    provider: Provider,
) {
    let cwd = get_repo_path(tree_id);
    let tools = all_tools(&cwd);

    // FileWatcher for external change detection
    let file_watcher = FileWatcher::start(&cwd)?;

    loop {
        // 1. Wait for user message on input_rx
        // 2. Read entries from store, build context
        // 3. Check for externally modified files (FileWatcher::check_changed())
        //    If files the agent previously read were modified externally, inject
        //    a system message noting the changes.
        // 4. Estimate token count BEFORE API call
        // 5. If estimate + max_tokens > context_window → trigger soft cap warning
        // 6. Call provider.stream_chat()
        let mut chat_stream = provider.stream_chat(&messages, &definitions)?;

        let mut response_text = String::new();
        let mut tool_calls_buf: Vec<ToolCallBuilder> = Vec::new();

        'stream: loop {
            let line = match chat_stream.next_line() {
                Some(l) => l,
                None => break,
            };
            if stop.load(Ordering::Relaxed) { break; }

            // Parse SSE "data: {json}"
            let data = line.strip_prefix("data: ").unwrap_or(&line);
            if data.trim() == "[DONE]" { break; }
            let chunk: ChatChunk = serde_json::from_str(data)?;
            let choice = match chunk.choices.first() {
                Some(c) => c, None => continue,
            };

            // ── Text delta: emit immediately to CLI ──
            if let Some(delta) = &choice.delta.content {
                response_text.push_str(delta);
                output_tx.send(ServerEvent::TextChunk {
                    content: delta.clone(),
                }).ok();
            }

            // ── Tool call delta: accumulate arguments ──
            if let Some(tool_deltas) = &choice.delta.tool_calls {
                for tc in tool_deltas {
                    let idx = tc.index.unwrap_or(0) as usize;
                    while tool_calls_buf.len() <= idx {
                        tool_calls_buf.push(ToolCallBuilder::default());
                    }
                    let builder = &mut tool_calls_buf[idx];
                    if let Some(id) = &tc.id { builder.id.clone_from(id); }
                    if let Some(name) = &tc.function.name { builder.name.clone_from(name); }
                    if let Some(args) = &tc.function.arguments {
                        builder.arguments.push_str(args);
                    }
                }
            }

            // ── Finish reason ──
            if let Some(reason) = &choice.finish_reason {
                match reason.as_str() {
                    "tool_calls" => {
                        for builder in &tool_calls_buf {
                            let call = ToolCall {
                                id: builder.id.clone(),
                                name: builder.name.clone(),
                                arguments: serde_json::from_str(&builder.arguments)?,
                            };
                            output_tx.send(ServerEvent::Entry(entry)).ok();

                        output_tx.send(ServerEvent::ToolStart {
                                tool: call.name.clone(),
                                input: call.arguments.clone(),
                            }).ok();

                            // Execute tool (with file lock, catch_unwind)
                            let tool = tools.iter()
                                .find(|t| t.definition().name == call.name)?;
                            let result = match std::panic::catch_unwind(|| {
                                tool.execute(&call.arguments)
                            }) {
                                Ok(Ok(output)) => output,
                                Ok(Err(e)) => ToolOutput {
                                    content: format!("Error: {}", e),
                                    truncated: false, original_size: 0, exit_code: Some(1),
                                },
                                Err(_) => {
                                    output_tx.send(ServerEvent::Error {
                                        message: format!("Tool '{}' panicked", call.name),
                                        fatal: false,
                                    }).ok();
                                    continue 'stream;
                                }
                            };

                            output_tx.send(ServerEvent::ToolResult {
                                tool: call.name.clone(),
                                exit: result.exit_code.unwrap_or(0),
                                output: if result.truncated {
                                    format!("{}... (truncated, was {} bytes)",
                                        &result.content[..result.content.len().min(2000)],
                                        result.original_size)
                                } else {
                                    result.content.clone()
                                },
                            }).ok();

                            // Append tool result to local message history
                            messages.push(Message {
                                role: MessageRole::Tool,
                                content: MessageContent::Text(if result.truncated {
                                    format!("{}... (truncated, was {} bytes)",
                                        &result.content[..result.content.len().min(2000)],
                                        result.original_size)
                                } else {
                                    result.content.clone()
                                }),
                                tool_calls: None,
                                tool_call_id: Some(call.id.clone()),
                                tool_name: Some(call.name.clone()),
                                usage: None,
                                stop_reason: None,
                                is_error: if result.exit_code.unwrap_or(0) != 0 { Some(true) } else { None },
                            });
                        }
                        tool_calls_buf.clear();
                        // Loop back to step 5 — LLM gets another turn with tool results
                        break 'stream; // restart the outer loop
                    }
                    "stop" => {
                        output_tx.send(ServerEvent::Done { status: "complete" }).ok();
                        break 'stream;
                    }
                    "length" => {
                        output_tx.send(ServerEvent::Done { status: "length" }).ok();
                        break 'stream;
                    }
                    _ => {}
                }
            }
        }

        // 8. Check total_tokens vs soft/hard cap
        // 9. At cap → break agent loop (server handles summarization separately)
        //    The server will call generate_continuation_brief() with just
        //    the session messages, then append session_end to the tree.
        //    See "Continuation Brief Generation" above.
    }
}
```

### Streaming Detail

| Event | When | CLI action |
|-------|------|------------|
| `TextChunk` | Per LLM response delta | Append to current line, no newline |
| `ToolStart` | Tool call detected mid-stream | Print `🛠 tool: args` on new line |
| `ToolResult` | Tool finished (sync) | Print indented output with `│` prefix |
| `TextChunk` | (resumes after tool result) | Append to line again |
| `Entry(Message)` | Message persisted to tree | (Skipped via dedup — already rendered from TextChunks) |
| `Entry(BashExec)` | Tool result persisted | (Skipped via dedup — already rendered from ToolStart/ToolResult) |
| `Entry(GoalSet)` | Goal changed | Print `🎯 New goal: ...` |
| `Entry(ModelSet)` | Model changed | Print `🤖 Model switched to ...` |
| `Entry(SessionEnd)` | Session segment ended | Print summary line if present |
| `CapWarning` | Context cap approaching | Print `⚠ Context at {pct}%` in yellow |
| `Done` | Turn complete | Newline, show prompt |

The CLI never waits for the full response — it paints `TextChunk` events as they
arrive via the SSE thread. This gives the user instant feedback even on slow LLMs.
```

---

## Session Lifecycle

```
Tree created (first user message)
  │
  ├── session_start ─── messages accumulate...
  │       ├── Soft cap (~65%) — warning to user/agent
  │       ├── Hard cap (~85%) — agent loop breaks cleanly
  │       └── Server makes separate summarization call → continuation brief
  │           (call uses only session messages as input, not full context)
  │
  ├── session_end (brief, status)
  │
  ├── session_start (new segment, header total_tokens reset)
  │       └── continuation brief injected at top of system prompt
  │           messages accumulate again...
  │
  └── ...
```

The ~20% gap between soft and hard cap is the agent's **output budget** — room
for the agent to keep producing useful work (code, analysis) rather than
meta-work. Context windows limit input + output tokens per request; the gap
gives the agent space for its final response before hitting the hard limit.
The continuation brief is generated by a separate LLM call with its own context
window (just the session's messages as input, much smaller), so it doesn't eat
into the agent's working budget.

---

## Configuration & Lifecycle

### Config File (`~/.agent/config.toml`)

Server and agent settings live in a single TOML file:

```toml
[server]
host = "127.0.0.1"
port = 8080

[provider]
base_url = "http://localhost:8080/v1"  # llama.cpp default
api_key = ""                             # or env: LLM_API_KEY
model = "qwen2.5-coder-7b-instruct"

# Separate summarization model (optional, defaults to provider.model)
# Can be a cheaper/faster model since summarization is lightweight.
[summary]
# Uses the same base_url and api_key as [provider] unless overridden here.
model = "qwen2.5-coder-1.5b-instruct"
base_url = "http://localhost:8080/v1"    # optional, falls back to provider.base_url
api_key = ""                              # optional, falls back to provider.api_key

[session]
soft_cap_pct = 65
hard_cap_pct = 85
max_tool_calls_per_turn = 25    # prevent infinite loops

[logging]
level = "info"                    # error | warn | info | debug
to_file = "/tmp/agent-server.log"
to_stderr = true
```

Config is loaded at startup and can be overridden by environment variables
(`AGENT_SERVER_PORT`, `LLM_API_KEY`, `AGENT_MODEL`, etc.).

### Logging

Two lightweight crates cover the logging needs:

- **`log`** — crate facade (de facto standard for Rust, zero deps on its own).
  Used via `log::info!()`, `log::warn!()`, etc.
- **`env_logger`** — stderr logger controlled by `RUST_LOG` env var.
  Lightweight, no extra config needed, widely used.

For file logging, a simple custom logger (~50 lines) that implements `log::Log`
and writes to a rotating file path. No extra crate required.

```rust
use log::{Record, LevelFilter, SetLoggerError, Metadata};
use std::fs::OpenOptions;
use std::io::Write;

struct FileLogger {
    file: Mutex<std::fs::File>,
    level: LevelFilter,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }
    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let mut f = self.file.lock().unwrap();
            writeln!(f, "{} {} {}", chrono::Utc::now(), record.level(), record.args()).ok();
        }
    }
    fn flush(&self) {
        self.file.lock().unwrap().flush().ok();
    }
}
```

Both loggers are registered at startup. `env_logger` is initialized via its
`Builder` (controlled by `RUST_LOG` env var), and the custom `FileLogger`
is installed as an additional logger via `log::set_boxed_logger()`:

```rust
// In server startup:
env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    .init();

// Also register file logger
let file_logger = FileLogger::new("/tmp/agent-server.log", LevelFilter::Info);
log::set_boxed_logger(Box::new(file_logger)).ok();
```

Thread-local logging prefix via `std::thread_local!`:

```rust
use std::cell::Cell;

thread_local! {
    pub static AGENT_TREE_ID: Cell<Option<String>> = const { Cell::new(None) };
}

// Set at agent thread start:
AGENT_TREE_ID.set(Some(tree_id.to_string()));

// In the custom FileLogger::log():
if let Some(tree_id) = AGENT_TREE_ID.with(|id| id.clone()) {
    writeln!(f, "[{} {} {} tree={}] {}", 
        chrono::Utc::now(), record.level(), record.args(), tree_id).ok();
} else {
    writeln!(f, "{} {} {}", chrono::Utc::now(), record.level(), record.args()).ok();
}
```

The env_logger stderr output does NOT include the tree prefix (it uses its own
formatting). The file logger includes it. Each agent thread gets a thread-local
prefix for correlation:

```
[2026-05-16T10:30:00Z INFO  agent::lifecycle] Spawning agent for tree abc-123
[2026-05-16T10:30:01Z INFO  agent::agent   tree=abc-123] Building context (est. 45k tokens)
[2026-05-16T10:30:05Z WARN  agent::agent   tree=abc-123] Soft cap at 68%
[2026-05-16T10:30:10Z INFO  agent::provider tree=abc-123] LLM call took 3.2s, 1200 output tokens
[2026-05-16T10:30:15Z INFO  agent::store   tree=abc-123] Appended session_end
```

### Graceful Shutdown

On SIGTERM/SIGINT:
1. Stop accepting new HTTP requests
2. Signal all active agent threads via `stop` flag
3. Wait for each agent to finish its current tool call (timeout: 60s)
4. For each interrupted agent, append `session_end` with `Aborted` status
5. Rebuild index from `.meta.json` files
6. Exit

Tools that panic are caught with `catch_unwind` in the agent entry point.
The error is logged, a `session_end` with `Blocked` is appended, and the
agent thread exits cleanly.

---

## Server (`agent-server/src/main.rs`)

### Route Handlers (`agent-server/src/routes.rs`)

```
GET    /trees                         list trees
POST   /trees                         create tree
  Body: { "title": "...", "repo_path": "...", "model": "..." }
  Note: repo_path is canonicalized on creation and immutable afterward.
  Note: The optional `model` field is NOT stored on TreeMeta (which has no model
  field). Instead, the server immediately writes a `ModelSet` entry as the first
  entry in the tree file (after `session_start`), so the model is tracked via the
  entry tree. TreeHeader.current_model is updated from the most recent ModelSet.
GET    /trees/{id}                    tree detail (meta + recent session info)
PATCH  /trees/{id}                    update title
  Body: { "title": "..." }
POST   /trees/{id}/message            send user message
  Body: { "text": "..." }
POST   /trees/{id}/stop               interrupt active agent
GET    /trees/{id}/stream             SSE stream for active agent
GET    /trees/{id}/entries            all entries (paginated)
```

No session endpoints — sessions are internal tree segments.

### Agent Lifecycle (`agent-server/src/lifecycle.rs`)

```rust
pub struct AgentHandle {
    pub thread: thread::JoinHandle<()>,
    pub input_tx: mpsc::Sender<AgentInput>,
    pub stop: Arc<AtomicBool>,
    // SSE event broadcast (used by SSE streaming, see section below)
    pub event_buffer: Arc<Mutex<VecDeque<ServerEvent>>>,
    pub event_broadcast: Arc<Mutex<Vec<mpsc::Sender<ServerEvent>>>>,
}

pub static ACTIVE_AGENTS: LazyLock<Mutex<HashMap<TreeId, AgentHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn spawn(tree_id: &str, provider: Provider) -> Result<()>;
pub fn stop(tree_id: &str) -> Result<()>;
pub fn send_message(tree_id: &str, text: &str) -> Result<()>;
```

### SSE Streaming with Reconnection

Each agent handle holds a ring buffer of the last N Entry events plus a live
broadcast channel. On SSE connect, the client gets buffer catch-up first,
then live events. This allows reconnection mid-session.

**Thread pool note:** The `SseReconnectStream` below blocks a rouille handler
thread on `recv()` while waiting for events. Rouille's pool has ~16 threads.
At >=16 concurrent SSE connections all pool threads would be blocked, stalling
new HTTP requests. For personal use (1 user, 1-2 connections) this is never a
problem. If scaling beyond that, swap to a dedicated-thread-per-client approach.

```rust
const BUFFER_CAPACITY: usize = 1000;

// Event emission from agent thread
fn emit_event(handle: &AgentHandle, event: ServerEvent) {
    // Ring buffer for reconnection catch-up (Entry events only)
    if matches!(event, ServerEvent::Entry(_)) {
        let mut buf = handle.event_buffer.lock().unwrap();
        if buf.len() >= BUFFER_CAPACITY { buf.pop_front(); }
        buf.push_back(event.clone());
        drop(buf);
    }

    // Live broadcast via mpsc channel - agent never blocks on SSE delivery
    let mut subs = handle.event_broadcast.lock().unwrap();
    subs.retain(|tx| tx.send(event.clone()).is_ok());
}

// Simple Read impl over mpsc::Receiver. from_stream_body spawns a thread
// internally in rouille, so the blocking recv() is NOT on the handler pool.
pub struct SseReconnectStream {
    catch_up: std::vec::IntoIter<ServerEvent>,
    rx: std::sync::mpsc::Receiver<ServerEvent>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for SseReconnectStream {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            let event = if let Some(e) = self.catch_up.next() {
                e
            } else {
                match self.rx.recv() {
                    Ok(e) => e,
                    Err(_) => return Ok(0),
                }
            };
            self.buf = format!("data: {}\n\n",
                serde_json::to_string(&event).unwrap()).into_bytes();
            self.pos = 0;
        }
        let n = (&self.buf[self.pos..]).read(dst)?;
        self.pos += n;
        Ok(n)
    }
}

fn stream_response(tree_id: &str) -> rouille::Response {
    let agents = ACTIVE_AGENTS.lock().unwrap();
    let handle = agents.get(tree_id).cloned().unwrap();
    drop(agents);

    let catch_up: Vec<ServerEvent> = {
        let buf = handle.event_buffer.lock().unwrap();
        buf.iter().cloned().collect()
    };

    let (tx, rx) = std::sync::mpsc::channel();
    handle.event_broadcast.lock().unwrap().push(tx);

    rouille::Response::from_stream_body("text/event-stream",
        SseReconnectStream { catch_up: catch_up.into_iter(), rx })
}
```

Multiple concurrent SSE clients are allowed (no 409). Clients that
disconnect are pruned on the next `emit_event` call via the `retain`.

---

## CLI (`agent-cli`)

The CLI is a separate binary that talks to the server over HTTP.

### Subcommands (`agent-cli/src/main.rs`)

```
agent-cli <command> [args]

Commands:
  serve                       Start the server daemon
  trees                       List trees
  create <title>              Create a new tree
  msg <tree_id> <message>     One-shot: send message, display stream, exit
  stop <tree_id>              Stop active agent
                              (no command)  Interactive TUI
```

### Interactive TUI (`agent-cli/src/interactive.rs`)

1. Connect to server (default `http://localhost:8080`)
2. `GET /trees` → user selects or creates a tree
3. Show prompt → user types message
4. `POST /trees/{id}/message` → `GET /trees/{id}/stream`
5. Receive SSE events in background thread, render to terminal via `termion`
6. Agent finishes → return to prompt

Two threads:

```
Main thread:  state machine, prints output (termion ANSI), reads stdin
SSE thread:   ureq GET /trees/{id}/stream, parse "data: {json}\n\n" lines,
              push ServerEvent to mpsc queue
```

Main thread alternates between draining the SSE queue and polling stdin
(via `termion::async_stdin` or a select-like poll with `thread::sleep`).

### HTTP Client (`agent-cli/src/client.rs`)

```rust
pub fn list_trees() -> Result<Vec<TreeMeta>>;
pub fn create_tree(title: &str) -> Result<TreeMeta>;
pub fn get_tree(id: &str) -> Result<TreeMeta>;
pub fn send_message(tree_id: &str, text: &str) -> Result<()>;
pub fn stop_agent(tree_id: &str) -> Result<()>;
pub fn stream_events(tree_id: &str) -> Result<impl Iterator<Item = ServerEvent>>;
```

### Rendering

```
$ agent-cli
Connected to server at http://localhost:8080

Your trees:
  [1] abc-123 — Refactor auth module (active)
  [2] def-456 — Add CI pipeline (completed)

Select a tree (or create new): 1
Now talking in: Refactor auth module
──────────────────────────────────────────
  ●  Let me look at the project structure...
  🛠  bash: ls -la
     │ total 42
     │ drwxr-xr-x ... src/
  ⚠  Context at 68% (soft cap)
  ●  Done. Here's what I changed...
──────────────────────────────────────────
[Refactor auth module] █
```

Layout rules:
- **Streaming output**: printed as it arrives
- **Tool start**: tool name + abbreviated input
- **Tool result**: indented with `│` prefix, truncated if long
- **Agent text**: `●` prefix
- **Cap warnings**: yellow `⚠`
- **Errors**: red
- **Prompt**: `[tree_name] ` at bottom, restored after each agent turn

### Commands (at prompt)

| Command | Description |
|---|---|
| `/trees` | List trees |
| `/create <title>` | Create new tree |
| `/switch <id>` | Switch to different tree |
| `/stop` | Stop active agent |
| `/show` | Show current tree info |
| `/entries [n]` | Show last N entries |
| `/help` | List commands |
| `/quit` | Exit |

### Queued Input

If the user types while the agent is working, input is buffered. On Enter,
it shows as "pending." When the agent finishes its current turn, queued
messages are sent.

---

## Error Handling

### LLM API Errors

| Scenario | Behavior |
|---|---|
| 4xx (auth, rate limit) | Abort session. Fatal error SSE. Append `session_end` with `Blocked`. |
| 5xx (server error) | Retry with exponential backoff (3 attempts). If all fail, abort. |
| Timeout | Same retry strategy. |
| Malformed SSE chunk | Skip, log, continue. If no valid response after stream, retry. |

### Tool Execution Errors

| Scenario | Behavior |
|---|---|
| Tool returns `Err` | Return structured error as tool result. Agent can retry. |
| Bash non-zero exit | Return stdout, stderr, exit code normally — valid data. |
| Tool timeout (>60s) | Kill child process. Return timeout error. |
| `edit` old_text not found | Fall back to fuzzy match (NFKC normalize, strip trailing whitespace, normalize quotes/dashes/spaces). If still not found, return structured error with description of what was attempted. Agent re-reads and retries. |
| Same tool fails 3 consecutive times | Escalate: emit error SSE, surface to user, abort tool loop for this turn. |

### Agent Loop Guards

| Guard | Limit | Behavior |
|---|---|---|
| Max tool calls per turn | 25 (configurable) | After 25 tool calls in one turn, force-return control to user. Prevents infinite loops. |
| Turn timeout | 300s (configurable) | If a single turn (LLM call + tools) exceeds this, interrupt and surface error. |
| Consecutive tool failures | 3 | Escalate to user instead of letting the agent retry blindly. |

### Server / Thread Crashes

| Scenario | Behavior |
|---|---|
| Agent thread panics | `catch_unwind` in agent entry point. Emit error SSE. Append `session_end` with `Blocked`. |
| Server restart | Scan trees for dangling session segments. Inject synthetic `session_end` with `Blocked`. |
| SSE client disconnects | Agent thread continues. New client can reconnect to stream. |
| JSONL corruption | `serde_json` parse error → skip line, log, continue. |

---

## File Change Detection

A filesystem watcher runs alongside the agent loops, using the [`notify`](https://crates.io/crates/notify)
crate (de facto standard for Rust filesystem events, uses inotify on Linux,
FSEvents on macOS, ReadDirectoryChanges on Windows).

The watcher monitors the current repo directory for file modifications and
serves two purposes:

**1. PWA file view.** When a file changes, push a `file_changed` SSE event with
the file path. The PWA re-fetches the file via `/fs/file` and shows a green
flash on the changed lines.

**2. Agent awareness.** Track which files the agent reads during each turn.
Before the agent's next LLM call, check if any of those files received
modification events. If so, inject a system message:

```
Note: The following files were modified externally after the agent read them:
  - src/main.rs
  - Cargo.toml

The agent should re-read these files before making further changes.
```

```rust
use notify::{Watcher, RecursiveMode, Event};
use std::sync::mpsc;

pub struct FileWatcher {
    _watcher: Box<dyn Watcher>,        // keep alive
    rx: mpsc::Receiver<Event>,         // filesystem events
    read_files: Mutex<HashSet<PathBuf>>,  // files the agent read this turn
}

impl FileWatcher {
    pub fn start(repo: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(repo, RecursiveMode::Recursive)?;
        Ok(Self { _watcher: Box::new(watcher), rx,
                  read_files: Mutex::new(HashSet::new()) })
    }

    /// Call when a tool reads a file.
    pub fn note_read(&self, path: &Path) {
        self.read_files.lock().unwrap().insert(path.canonicalize()?);
    }

    /// Drain events. Return files the agent read that changed.
    pub fn check_changed(&self) -> HashSet<PathBuf> {
        let read = self.read_files.lock().unwrap();
        let mut changed = HashSet::new();
        while let Ok(event) = self.rx.try_recv() {
            for path in &event.paths {
                if read.contains(path) { changed.insert(path.clone()); }
            }
        }
        changed
    }

    /// Start a new turn — clear the read set.
    pub fn new_turn(&self) { self.read_files.lock().unwrap().clear(); }
}
```

Architecture: the `notify` events come in on a separate OS thread (inotify
thread on Linux, FSEvents thread on macOS). They are drained synchronously
by the agent loop between turns via `check_changed()`. No extra threads
needed in the server — the notify crate handles the platform thread internally.

Both the agent awareness and PWA SSE events draw from the same event stream.

---

## Implementation Plan

Concrete step-by-step build order. Each step is designed to be a single agent work session
(fresh context) that produces working, testable code. At the end of each step, update
`IMPLEMENTATION_NOTES.md` (append a new `## Step N` section) with:

- `[x]` mark next to the step name
- Key decisions made, deviations from PLAN.md, and rationale
- Any bugs fixed or edge cases discovered
- Test command used to verify

---

### Step 1 — Workspace skeleton and core types

**Goal:** Compiling workspace with all types defined.

**Files to create:**
- `agent/Cargo.toml` — workspace root (`resolver = \"2\"`)
- `agent/agent-core/Cargo.toml` — library crate
- `agent/agent-core/src/lib.rs` — pub mod declarations
- `agent/agent-core/src/types.rs` — all types from PLAN.md:
  - `TreeId`, `EntryId` type aliases
  - `TreeMeta` with all fields
  - `TreeHeader`
  - `Entry` enum (SessionStart, SessionEnd, Message, BashExec, ModelSet, Label, GoalSet)
  - `Message`, `MessageRole`, `MessageContent`, `ContentBlock`
  - `ToolCall`, `ToolCallBuilder`
  - `StopReason`, `TokenUsage`, `SessionStatus`
  - `AgentInput` enum
  - `ChatChunk`, `Choice`, `Delta`, `DeltaToolCall`
  - `ChatStream` struct
  - `ServerEvent` enum — all variants
  - `ToolDefinition`, `ToolOutput`
- `agent/agent-server/Cargo.toml` — binary crate
- `agent/agent-server/src/main.rs` — stub
- `agent/agent-cli/Cargo.toml` — binary crate
- `agent/agent-cli/src/main.rs` — stub

**Verify:** `cargo build --workspace` compiles with no warnings.

---

### Step 2 — Config + logging + store (JSONL I/O)

**Goal:** Persistent storage layer: create/read/append trees.

**Files to create/modify:**
- `agent/agent-core/src/config.rs` — load `~/.agent/config.toml`, env var overrides
- `agent/agent-core/src/logging.rs` — `FileLogger`, thread-local `AGENT_TREE_ID`, dual logger init
- `agent/agent-core/src/store.rs` — JSONL I/O:
  - `create_tree_file()`, `append_entry()`, `read_all_entries()`
  - `load_tree_meta()`, `save_tree_meta()`, `list_trees()`, `get_tree()`
  - `rebuild_index()`, `update_header()`
  - Static `INDEX_CACHE`, `FILE_LOCKS`
  - Atomic write pattern (`write`+`rename`)
- Wire modules in `lib.rs`

**Verify:** `cargo test` with a test that creates tree + append + read round-trips.

---

### Step 3 — Provider (LLM API client)

**Goal:** Streaming chat completions via OpenAI-compatible API.

**Files to create/modify:**
- `agent/agent-core/src/provider.rs`:
  - `Provider` struct (base_url, api_key, model)
  - `stream_chat(messages, tools) -> Result<ChatStream>` — ureq POST to `/v1/chat/completions`
  - `generate_continuation_brief(messages) -> Result<(String, SessionStatus)>`
- Wire in `lib.rs`

**Verify:** Integration test against a mock HTTP server or real llama.cpp.

---

### Step 4 — Tool system (trait + initial tools)

**Goal:** Tool trait, registry, and first 6 tools working.

**Files to create:**
- `agent/agent-core/src/tools/mod.rs` — `Tool` trait, `ToolDefinition`, `ToolOutput`, `all_tools()`
- `agent/agent-core/src/tools/read.rs` — `ReadTool` (2000 lines / 50 KB limit)
- `agent/agent-core/src/tools/write.rs` — `WriteTool`
- `agent/agent-core/src/tools/ls.rs` — `LsTool`
- `agent/agent-core/src/tools/grep.rs` — `GrepTool`
- `agent/agent-core/src/tools/find.rs` — `FindTool`
- `agent/agent-core/src/tools/git.rs` — `GitTool` (status, diff, log, show, add, commit, push, pull)

**Delayed (Step 6):** edit, bash, search

**Verify:** `cargo test` runs per-tool tests with tempfile fixtures.

---

### Step 5 — Server skeleton + tree CRUD routes

**Goal:** Server boots, serves tree CRUD API.

**Files to create/modify:**
- `agent/config.toml` — default dev config (committed)
- `agent/agent-server/src/main.rs` — init config, logging, store, start rouille
- `agent/agent-server/src/routes.rs`:
  - `GET /trees`, `POST /trees`, `GET /trees/{id}`, `PATCH /trees/{id}`, `GET /trees/{id}/entries`
  - JSON request/response bodies
- `agent/agent-server/src/lifecycle.rs` — `AgentHandle`, `ACTIVE_AGENTS` map, `spawn()`/`stop()` stubs

**Verify:** `cargo run -p agent-server` boots, `curl localhost:8080/trees` returns `[]`.

---

### Step 6 — Edit + Bash + Search tools

**Goal:** The three most complex tools working.

**Files to create:**
- `agent/agent-core/src/tools/edit.rs` — exact + fuzzy match, per-file locking, multi-edit
- `agent/agent-core/src/tools/bash.rs` — process group kill, timeout, output limits
- `agent/agent-core/src/tools/search.rs` — `search_messages()`, `search_files()` via stream deserialization
- Wire all 3 into `all_tools()`

**Verify:** `cargo test` for edit fuzzy matching, bash timeout, lock contention.

---

### Step 7 — Agent loop + context building

**Goal:** Agent thread receives input, builds context, calls LLM, dispatches tools, emits events.

**Files to create/modify:**
- `agent/agent-core/src/context_files.rs` — `load_context_files()` walks up from cwd
- `agent/agent-core/src/agent.rs`:
  - `build_context(entries, leaf_id) -> Vec<Message>` — walk parent chain
  - `estimate_tokens()`, `estimate_context_tokens()`
  - `run_agent()` — main loop: wait for input → build context → stream_chat → parse chunks → emit events → dispatch tools → repeat
  - Max tool calls per turn guard (25)
- `agent/agent-core/src/hooks.rs` — `Hook` trait, registry, `run_tool_call_hooks()`, `run_before_llm_hooks()`
- `agent/agent-server/src/lifecycle.rs` — wire `spawn()` to launch agent thread

**Verify:** `cargo test --lib` for context building logic.

---

### Step 8 — SSE streaming + event broadcast

**Goal:** Clients subscribe to agent events with reconnection support.

**Files to modify:**
- `agent/agent-server/src/routes.rs` — add `GET /trees/{id}/stream` with `SseReconnectStream`
- `agent/agent-server/src/lifecycle.rs`:
  - Add `event_buffer` (ring buffer, 1000 cap) and `event_broadcast` to `AgentHandle`
  - `emit_event()` helper — ring buffer + broadcast to all subscribers
- Wire `emit_event` calls in agent loop

**Verify:** `curl -N localhost:8080/trees/{id}/stream` receives SSE events.

---

### Step 9 — CLI: HTTP client + interactive TUI

**Goal:** Full CLI to create trees, send messages, watch streaming output

**Files to create:**
- `agent/agent-cli/src/client.rs` — ureq-based HTTP client helpers
- `agent/agent-cli/src/interactive.rs`:
  - Two-thread: SSE thread → mpsc, main thread renders + polls stdin
  - Tree select/create → prompt loop
  - termion rendering (`●`, `🛠`, `│`, `⚠` prefixes, dedup by EntryId)
  - Commands: `/trees`, `/create`, `/switch`, `/stop`, `/show`, `/entries`, `/help`, `/quit`
  - Queued input while agent works
- `agent/agent-cli/src/main.rs` — clap subcommands

**Verify:** `cargo run -p agent-cli trees` lists trees; interactive mode sends message and streams.

---

### Step 10 — File watcher + graceful shutdown

**Goal:** File change detection and clean shutdown.

**Files to create/modify:**
- `agent/agent-core/src/file_watcher.rs` — notify-based watcher, `note_read()`, `check_changed()`, `new_turn()`
- `agent/agent-server/src/main.rs` — signal handlers (SIGTERM/SIGINT), drain agents, write session_end, rebuild index

**Verify:** SIGINT during active tool call → Aborted session_end written.

---

### Step 11 — Session lifecycle with continuation briefs

**Goal:** Soft/hard cap triggers, auto session_end + brief generation.

**Files to modify:**
- `agent/agent-core/src/agent.rs` — token cap check after each LLM response, emit CapWarning
- `agent-server/src/lifecycle.rs` — on hard cap: collect session messages, call `generate_continuation_brief()`, write SessionEnd + SessionStart
- `agent/agent-core/src/store.rs` — `reset_header_tokens()`

**Verify:** Hard cap reached → continuation brief written → next session injects it.

---

### Step 12 — Hooks integration + polish

**Goal:** Hook sites wired, error handling robust, clippy-clean.

**Files to modify:**
- `agent/agent-core/src/hooks.rs` — wire all hook sites in agent loop
- Load hooks from config
- LLM retry logic (3 attempts, exponential backoff)
- Tool failure escalation (3 consecutive → abort)
- Final `cargo clippy --workspace` clean, `cargo test --workspace` green

**Verify:** Full integration: create → message → read → edit → bash → soft cap → hard cap → brief survives.

---

### Step 13 — (Future) PWA frontend

**Goal:** Mobile browser client.

Not detailed — deferred until CLI/SSE API is stable from real use.

---

## Deferred

- PWA / browser frontend
- Voice input (Web Speech API)
- Autonomous ralph loop (agent self-continues without user)
- Subtask spawning (child trees via `parent_id`)
- Provider abstraction beyond simple config struct
- Tree archival / cleanup

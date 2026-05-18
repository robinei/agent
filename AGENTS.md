# Agent Server

Personal coding agent server with a CLI client. Rust workspace, synchronous threaded (no async runtime).

## Architecture

3 crates in a Cargo workspace:

- **`agent-core`** — library: types, JSONL store, LLM provider, tools (read/write/edit/bash/grep/find/ls/git/search), agent loop, context files, hooks
- **`agent-server`** — binary: rouille HTTP server, routes, agent lifecycle (spawn/kill/SSE broadcast)
- **`agent-cli`** — binary: interactive TUI (termion) + one-shot commands via clap

## Build & Run

- `cargo build --workspace`
- `cargo test --workspace` — run all tests
- `cargo clippy --workspace`
- Server: `cargo run -p agent-server`
- CLI interactive: `cargo run -p agent-cli`
- CLI one-shot: `cargo run -p agent-cli -- msg <tree_id> "your message"`

## Key Conventions

- **No async.** Use `std::thread`, `std::sync::mpsc`, `Mutex`.
- **Tools implement `Tool` trait** in `agent-core/src/tools/`. Register in `all_tools()` in `mod.rs`. One file per tool.
- **Tools use real filesystem** (no I/O trait abstractions). Tests create temp dirs with `tempfile::TempDir`.
- **File mutations** (edit/write) acquire a per-file mutex via `with_file_lock()` to serialize concurrent writes.
- **JSONL storage** under `~/.agent/`. Tree = `{uuid}.jsonl` (header + entries) + `{uuid}.meta.json`.
- **SSE streaming** from server to CLI: `TextChunk`, `ToolStart`, `ToolResult`, `Entry(...)`, `CapWarning`, `Done`, `Error`.
- **Events** are `ServerEvent` enum in `agent-core/src/types.rs`. Agent emits events via mpsc channel, server broadcasts to SSE subscribers.

## Adding a Tool

1. Create `tools/my_tool.rs` — implement `Tool` trait (fn `definition()` + fn `execute()`)
2. Add one line in `all_tools()` in `tools/mod.rs`
3. Test with temp dir + fixture files

## Patterns

- `Entry` enum (tagged JSON via `#[serde(tag = "type")]`) — `Message`, `BashExec`, `SessionStart`, `SessionEnd`, `GoalSet`, `ModelSet`, `Label`
- `Store` wrapped in `Arc` for cross-thread sharing
- Agent loop in `agent.rs`: wait for message → `build_context()` → `provider.stream_chat()` → parse SSE chunks → dispatch tools → persist → emit ServerEvents
- Context building: walk parent chain from `leaf_id` up, stop at `SessionEnd` with `continuation_brief`, insert `GoalSet`/`ModelSet` as system messages, skip `bash_exec` entries (already reflected in tool result messages)
- Token heuristic: `ceil(len / 3.5)` for context estimation before API call

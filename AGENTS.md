# Agent Server

Personal coding agent server with a CLI client. Rust workspace, synchronous
threaded (no async runtime). Each tree runs in its own bubblewrap-sandboxed
worker subprocess; the server multiplexes clients over a single WebSocket
endpoint.

## Architecture

5 crates in a Cargo workspace:

- **`agent-core`** — lib: types, JSONL store, LLM provider, tools
  (read/write/edit/bash/grep/find/ls/git/search), agent loop, context files,
  hooks, WsCommand protocol (`rpc.rs`)
- **`agent-server`** — lib: hand-rolled HTTP layer on `TcpListener` +
  `httparse`, WebSocket upgrade via `tungstenite::WebSocket::from_raw_socket`,
  worker subprocess lifecycle, bwrap argv builder, graceful shutdown
- **`agent-worker`** — lib: stdin/stdout JSON bridge, hosts the agent loop
  inside a sandboxed subprocess
- **`agent-cli`** — lib: interactive TUI (`termion` raw mode), one-shot
  commands via `clap`, `tungstenite` WebSocket client
- **`agent`** — bin: dispatches `agent server | cli | worker` to the
  corresponding lib's `run()` entry point

```
CLI (tungstenite) ── WS ──▶ Server (one TcpListener, one port)
                              │
                              ├── HTTP routes (tree CRUD, all JSON, Connection: close)
                              │
                              └── WS upgrade per tree
                                     │
                                     ▼
                              Worker subprocess (bwrap-sandboxed)
                              stdin: JSON command lines
                              stdout: JSON ServerEvent lines
                              stderr: demuxed to server log
```

One worker process per active tree, lives across many turns. WS thread on the
server owns the socket exclusively (non-blocking + 10ms poll). A stdout-proxy
thread reads worker events and fans them out to all WS subscribers + a
per-tree ring buffer.

## Build & Run

```bash
# One binary, three subcommands
cargo build --workspace
cargo test --workspace
cargo clippy --workspace

# Run via cargo
cargo run -p agent server
cargo run -p agent cli              # interactive TUI
cargo run -p agent cli msg <id> "..." # one-shot

# Or the built binary directly
./target/debug/agent server
./target/debug/agent cli
```

See `DEBUG.md` for env-var configuration and troubleshooting.

## Key conventions

- **No async runtime.** `std::thread`, `std::sync::mpsc`, `Mutex`. No tokio.
- **Tools implement `Tool`** in `agent-core/src/tools/`. Register in
  `all_tools()` in `mod.rs`. One file per tool.
- **Tools use the real filesystem** (no I/O trait abstractions). Tests create
  temp dirs with `tempfile::TempDir`.
- **File mutations** acquire a per-file mutex via `with_file_lock()` to
  serialize concurrent writes inside a process.
- **Storage:** `~/.agent/trees/{uuid}/data.jsonl` (header on line 1, then
  append-only entries) + `~/.agent/trees/{uuid}/meta.json` (atomic
  rename-over-write).
- **Server is the sole writer of `meta.json`.** Workers communicate desired
  meta changes via events; the server applies them. This is what keeps a
  rogue worker from rewriting its own `repo_path` or sandbox config to
  escalate privileges on the next spawn.
- **WS protocol:** see `agent-core/src/rpc.rs` for `WsCommand`. Each frame is
  one JSON object: `{"method":"message","params":{"text":"..."}}` or
  `{"method":"stop"}`.
- **Server events:** `ServerEvent` enum in `agent-core/src/types.rs` —
  `TextChunk`, `ToolStart`, `ToolResult`, `Entry(...)`, `CapWarning`,
  `MetaUpdate`, `Done`, `Error`. The agent emits via mpsc; the server's WS
  thread forwards to subscribed clients.

## Sandbox model

- Default-deny on writes (just the tree's `repo_path` and its
  `~/.agent/trees/{id}/` dir), default-allow on net, credential dirs
  tmpfs'd via `[sandbox.defaults]`.
- Per-tree overrides in `TreeMeta.sandbox` (`TreeSandbox` struct): `writable`,
  `network: Option<bool>`, `hide`, `unhide`.
- bwrap argv built in `agent-server/src/lifecycle.rs::build_bwrap_argv`.
- Set `[sandbox] enabled = false` in `~/.agent/config.toml` for unsandboxed
  spawns (development).

## Adding a tool

1. Create `agent-core/src/tools/my_tool.rs`. Implement `Tool` (`fn
   definition()`, `fn execute()`).
2. Add one line in `all_tools()` in `tools/mod.rs`.
3. Test with a temp dir + fixture files.

## Patterns

- **Entry enum** (tagged via `#[serde(tag = "entry_type")]`) —
  `Message`, `BashExec`, `SessionStart`, `SessionEnd`, `GoalSet`, `ModelSet`,
  `Label`.
- **Agent loop** (`agent-core/src/agent.rs::run_agent`): wait for message →
  `build_context()` → `provider.stream_chat()` → parse SSE chunks → dispatch
  tools → persist entries → emit `ServerEvent`s.
- **Context building** walks the parent chain from `leaf_id` upward,
  stopping at the first `SessionEnd` with a `continuation_brief`, inserts
  `GoalSet`/`ModelSet` as system messages, skips `BashExec` (already
  reflected in tool result messages).
- **Token heuristic:** `ceil(len / 3.5)` for context estimation; emit
  `CapWarning` at soft cap, force `session_end` at hard cap.
- **Stop is dual-path:** the worker's stdin reader thread sets an
  `Arc<AtomicBool>` *and* pushes `AgentInput::Stop` on the mpsc channel.
  The atomic is what the agent's tight inner loops check; without it,
  mid-LLM-stream stop is delayed until the stream ends.
- **Auto-title** is server-side: the stdout-proxy thread observes a `Done`
  event, checks `meta.title.is_none()`, and fires a side thread that calls
  `agent::auto_title`, updates meta, and broadcasts `MetaUpdate` to
  subscribers.
- **Crash recovery:** if a worker exits non-zero, the proxy thread emits
  `Error{fatal: true}` + `Done{status: "aborted"}` and removes the entry.
  On server startup, `store.scan_unterminated()` walks every tree and
  `lifecycle::recover_tree()` appends a linked synthetic `SessionEnd`
  for any whose last entry isn't one.
- **Hooks** (`agent-core/src/hooks.rs`): tool-call, before-LLM-call,
  session-end, startup. Workers and the server both run startup hooks.

## Plan

Pending and in-flight work is in `PLAN.md` — checked-off steps with
`**Notes:**` blocks below each. Append notes inline on completion.

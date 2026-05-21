# Agent Server

Personal coding agent server with a CLI client. Rust workspace, synchronous
threaded (no async runtime). Each tree runs in its own bubblewrap-sandboxed
worker subprocess; the server multiplexes clients over a single WebSocket
endpoint.

## Architecture

5 crates in a Cargo workspace:

- **`agent-core`** — lib: shared-only modules — types, JSONL store, config,
  hooks, logging, `rpc.rs` (WsCommand/PipeIn/LlmRequest/LlmResponse protocol)
- **`agent-server`** — lib: hand-rolled HTTP layer on `TcpListener` +
  `httparse`, WebSocket upgrade, single-threaded per-worker event loop
  (`worker_loop.rs`, `nix::poll`), LLM HTTP/TLS proxy (`llm_handler.rs`,
  `rustls`), worker subprocess lifecycle, bwrap argv builder, graceful
  shutdown, LLM provider (`provider.rs`), auto-title (`auto_title.rs`)
- **`agent-worker`** — lib: stdin/stdout JSON bridge, state-machine agent loop
  (`AgentState::Idle | Streaming`), tools
  (read/write/edit/bash/grep/find/ls/git/search), context building
  (`agent.rs`), thinking-tag parsing (`thinking.rs`), turn logic (`turn.rs`)
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
                              worker_loop (one thread, nix::poll)
                              ├── StdoutHandler  — reads worker stdout (JSON events)
                              ├── StderrHandler  — demuxes worker stderr to log
                              ├── WsClient(s)    — per connected CLI session
                              ├── LlmHandler     — rustls TLS socket, streams LLM response
                              └── NotifyHandler  — wakes loop on new WS writes
                                     │
                                     ▼
                              Worker subprocess (bwrap-sandboxed)
                              stdin: PipeIn JSON lines (Cmd | Llm | Config)
                              stdout: ServerEvent JSON lines
                              stderr: forwarded to server log
```

One worker process per active tree, lives across many turns. The server
manages one event-loop thread per worker using `nix::poll` over all relevant
fds — no per-client threads. LLM HTTP/TLS streaming is handled as a fd in
the same poll loop (`LlmHandler`, `rustls` state machine).

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
- **Tools implement `Tool`** in `agent-worker/src/tools/`. Register in
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
- **WS protocol:** see `agent-core/src/rpc.rs` for `WsCommand` (CLI→server),
  `PipeIn` (server→worker, wraps `WsCommand | LlmResponse | Config`),
  `LlmRequest`/`LlmResponse` (worker↔server LLM proxy). Each frame is one
  JSON object: `{"method":"message","params":{"text":"..."}}` or
  `{"method":"stop"}`.
- **Server events:** `ServerEvent` enum in `agent-core/src/types.rs` —
  `TextChunk`, `ToolStart`, `ToolResult`, `Entry(...)`, `CapWarning`,
  `MetaUpdate`, `Done`, `Error`. The worker writes these to stdout as JSON
  lines; the server's `StdoutHandler` fans them out to WS clients.

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

1. Create `agent-worker/src/tools/my_tool.rs`. Implement `Tool` (`fn
   definition()`, `fn execute()`).
2. Add one line in `all_tools()` in `tools/mod.rs`.
3. Test with a temp dir + fixture files.

## Patterns

- **Entry enum** (tagged via `#[serde(tag = "entry_type")]`) —
  `Message`, `BashExec`, `SessionStart`, `SessionEnd`, `GoalSet`, `ModelSet`,
  `Label`.
- **Agent loop** (`agent-worker/src/lib.rs`): state machine on `PipeIn` lines
  from stdin — `AgentState::Idle` receives `Cmd::Message` → calls
  `begin_turn()` → emits `LlmRequest` → enters `AgentState::Streaming`.
  `Llm::Chunk` events drive `process_chunk()`; `Llm::Done` drives
  `finish_response()` (tool dispatch, re-entry, or `Done` event).
- **LLM proxy:** server's `LlmHandler` receives an `LlmRequest` from the
  worker's stdout, opens a TLS connection to the provider, streams SSE chunks
  back as `PipeIn::Llm(LlmResponse::Chunk)` lines to the worker's stdin.
- **Context building** (`agent-worker/src/agent.rs::build_context`): walks
  the parent chain from `leaf_id` upward, stopping at the first `SessionEnd`
  with a `continuation_brief`, inserts `GoalSet`/`ModelSet` as system
  messages, skips `BashExec` (already reflected in tool result messages).
- **Thinking tags:** `agent-worker/src/thinking.rs::split_thinking_chunks`
  strips `<think>…</think>` spans from streamed chunks; thinking content is
  suppressed from the stored transcript and WS output.
- **Token heuristic:** `ceil(len / 3.5)` for context estimation; emit
  `CapWarning` at soft cap, force `session_end` at hard cap.
- **Stop:** the server sends `PipeIn::Cmd(Stop)` on the worker's stdin.
  The worker's main loop checks the `stop` atomic before starting a new
  tool call round; mid-stream, the server can also drop the LLM connection.
- **Auto-title** is server-side (`agent-server/src/auto_title.rs`): the
  `StdoutHandler` observes a `Done` event, checks `meta.title.is_none()`,
  fires a side thread that calls `auto_title`, updates meta, and broadcasts
  `MetaUpdate` to subscribers.
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

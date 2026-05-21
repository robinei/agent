# Plan

Steps for ongoing work on the agent server. Architecture overview and
"how things fit together" live in `AGENTS.md`; troubleshooting recipes in
`DEBUG.md`. This file is only step specs (what to build next).

---

## Future ideas

Small things worth doing eventually; promote to a step when picked up.

- **File-change awareness.** Watch the repo directory with the `notify` crate.
  Track files the agent reads each turn; before the next LLM call, inject a
  system message listing any of those files that were modified externally.
  Same event stream can later push `FileChanged` events to a PWA. The
  `ServerEvent::FileChanged { path, kind }` variant already exists for this.
- **Queued input while the agent is working.** Buffer user input typed during
  streaming; flush on Enter as a pending message that sends once the current
  turn ends. (Cancellation covers the immediate-stop case; this is the
  friendlier "I have a follow-up" case.)
- **5xx retry with exponential backoff** in the provider client. Currently any
  provider error is fatal; three retries on transient 5xx would smooth over
  flaky upstream.
- **Turn timeout** (configurable, default ~300s) in the agent loop, alongside
  the existing `max_tool_calls_per_turn` guard.
- **Subtask spawning** — child trees linked via `parent_id`, so the agent can
  fork a sub-investigation that returns a summary into the parent.
- **Autonomous loop** — agent self-continues without user input on a cadence.
- **PWA frontend** — browser client over WS, voice input via Web Speech API.
- **Provider abstraction** beyond the current single-config struct (Anthropic,
  OpenAI, local OpenAI-compatible all supported via base_url today, but a
  trait would let us add response-shape adapters).

---

## Conventions (apply to every step)

- **Derives on new types:** `Serialize, Deserialize, Clone, Debug` always;
  add `Default` where an empty value is meaningful; add `PartialEq` only
  if tests need it.
- **New fields on existing serialized types:** `#[serde(default)]` so older
  on-disk JSON keeps deserializing.
- **Error style:** `Result<T, String>` for internal call sites matching the
  pattern in `agent-server/src/lifecycle.rs`; `thiserror` enums for
  `agent-core` library errors (matches `store.rs`, `provider.rs`).
- **Logging:** `log::info!` / `warn!` / `error!`. Prefix multi-component
  logs with a bracketed tag like `[lifecycle]`, `[worker]`, `[ws]`.
- **File I/O:** `std::fs::create_dir_all` before writes when the parent dir
  might not exist; atomic `rename` for any non-append write that must not
  be observed half-written.
- **Cargo.lock:** never hand-edit. After `Cargo.toml` changes, run
  `cargo build` to regenerate.
- **No async runtime.** No `tokio`, no `async-std`. `std::thread` and
  `std::sync::mpsc` only.
- **`#[allow(dead_code)]` is forbidden** as a way to silence warnings.
  Either use the code or delete it.
- **Tests live with the code:** `#[cfg(test)] mod tests { ... }` at the
  bottom of the file containing the unit under test. Integration tests
  go in a `tests/` directory at the crate root.
- **Transcribe explanatory comments from the spec into code.** Comments
  that explain *why* a line is the way it is — especially "INTENTIONAL:",
  "DO NOT...", or any rationale of "what would go wrong if you changed
  this" — should be copied verbatim into the implementation. They exist
  to stop a later reader (human or model) from "improving" the code into
  a bug.

---

## Step template

```
### <Name>

- [ ] todo / - [x] done

**Goal:** one or two sentences.

**Spec details:** file paths, signatures, tests, do-not-modify list.

**Verify:** commands that prove it works.
```

On completion: delete this entry, then commit code + PLAN.md together with:

```
<crate/area>: <brief title>

<what was built, 1-2 sentences>

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
```

---

## Steps

### Worker state machine refactor

- [x] Strip `agent-core/src/agent.rs` to library surface
- [x] Remove pipe infrastructure from `agent-core/src/provider.rs`
- [x] Remove `AgentInput` from `agent-core/src/types.rs`
- [x] Rewrite `agent-worker/src/lib.rs` as a state machine event loop
- [x] Fix imports in `agent-server/src/lifecycle.rs`
- [x] `cargo build && cargo test`

**Goal:** Replace the current worker architecture — which shares stdin between
two consumers (`next_input` closure and `SyncStdinChunkReader`) using skip
logic and a drain-on-drop hack — with a single stdin event loop driven by an
`AgentState` machine. Move the agent loop out of `agent-core` (where it does
not belong) and into `agent-worker`. `agent-core` becomes a pure library:
types, tools, store, config, hooks, provider.

**Why a state machine works cleanly here:** tool execution is synchronous and
never reads from the pipe, so no state transition needs to yield
mid-execution. The only blocking point is `reader.read_line()` at the top of
the loop. A `Cmd(Stop)` or `Cmd(Message)` arriving while LLM chunks are
streaming is just another match arm — no special skip logic needed.

---

#### `agent-worker/src/lib.rs` — full rewrite

**State enum:**

```rust
enum AgentState {
    Idle,
    Streaming {
        messages: Vec<Message>,       // full context, grows with each tool round
        leaf_id: Option<String>,
        response_text: String,        // accumulated text for this LLM call
        in_thinking: bool,            // <think> tag parser state across chunks
        tool_calls_buf: Vec<ToolCallBuilder>,
        finish_reason: Option<String>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
    },
}
```

**`run()` skeleton:**

```rust
pub fn run() -> Result<(), Box<dyn Error>> {
    let tree_id = parse_tree_id()?;

    // First stdin line must be PipeIn::Config.
    let mut reader = BufReader::new(std::io::stdin());
    let config = read_config(&mut reader)?;

    init_logging(...);
    run_startup_hooks();

    let store = Store::default();
    let session_cfg = SessionConfig { ... };
    let cwd = resolve_repo_path(&store, &tree_id);
    let tools = all_tools(&cwd);
    let stop = Arc::new(AtomicBool::new(false));
    let mut out = BufWriter::new(std::io::stdout());

    let mut state = AgentState::Idle;
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 { break; }
        let msg: PipeIn = match serde_json::from_str(line.trim_end()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg {
            PipeIn::Cmd(WsCommand::Message { params }) => {
                if matches!(state, AgentState::Idle) {
                    state = begin_turn(params.text, &tree_id, &store, &session_cfg,
                                       &tools, &cwd, &stop, &mut out);
                }
                // Streaming: silently drop — client must not send while agent is working.
            }
            PipeIn::Cmd(WsCommand::Stop) => {
                state = cancel_turn(state, &tree_id, &store, &stop, &mut out);
            }
            PipeIn::Llm(LlmResponse::Chunk { data, .. }) => {
                if let AgentState::Streaming { .. } = &mut state {
                    process_chunk(&data, &mut state, &mut out);
                }
            }
            PipeIn::Llm(LlmResponse::Done { .. }) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    state = finish_response(state, &tree_id, &store, &session_cfg,
                                            &tools, &stop, &mut out);
                    // Returns Streaming (another tool round) or Idle.
                }
            }
            PipeIn::Llm(LlmResponse::Error { message, .. }) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    emit_event(&mut out, ServerEvent::Error { message, fatal: true });
                    state = AgentState::Idle;
                }
            }
            PipeIn::Config(_) => {} // already initialized; ignore
        }
    }
    Ok(())
}
```

**`begin_turn`** — called when `Idle` and a `Cmd(Message)` arrives:

1. Read all entries from store; get tree meta and current `leaf_id`.
2. Call `build_context(&entries, leaf_ref)` to build message list.
3. Allocate new entry ID, persist user `Message` entry via `store.append_entry`,
   emit `ServerEvent::Entry`, update local `leaf_id`.
4. Load context files; prepend system prompt message (same text as today).
5. Run before-LLM hooks; if blocked, emit `Error { fatal: false }`, return `Idle`.
6. Estimate tokens; emit `CapWarning` if ≥ soft cap; if ≥ hard cap, call
   `write_session_end`, emit `Error { fatal: false }`, return `Idle`.
7. Write `PipeOut::Llm(LlmRequest { id: 0, messages: messages.clone(), tools: defs })`
   to stdout, flush.
8. Return `AgentState::Streaming { messages, leaf_id, response_text: String::new(),
   in_thinking: false, tool_calls_buf: vec![], finish_reason: None,
   tool_call_round: 0, tool_calls_this_turn: 0, consecutive_failures: 0 }`.

**`process_chunk`** — called for each `Llm(Chunk { data })` while `Streaming`:

- If `data` trims to empty, `":"` (SSE comment), or `"data: [DONE]"`: return
  immediately without updating state. `[DONE]` is an SSE convention; the
  `Llm(Done)` protocol message is the sole trigger for `finish_response`. This
  eliminates the drain-on-drop problem: when `finish_response` runs and sends
  the next `LlmRequest`, there is no orphaned `Llm(Done)` in stdin.
- Strip `"data: "` prefix; parse as `ChatChunk` (same serde types as today).
- Handle `choice.delta.reasoning` → emit `ThinkingChunk`.
- Handle `choice.delta.content` → call `split_thinking_chunks` → emit
  `TextChunk` / `ThinkingChunk`; accumulate non-thinking text into
  `response_text`.
- Accumulate `choice.delta.tool_calls` into `tool_calls_buf` by index.
- If `choice.finish_reason` is `Some` and non-empty, store in `finish_reason`.

**`finish_response`** — called on `Llm(Done)` while `Streaming`. Takes
ownership of state, returns new state.

Branch on `finish_reason`:

- `"tool_calls"`:
  1. Materialise `completed_calls: Vec<ToolCall>` from `tool_calls_buf`.
  2. Persist assistant `Message` entry (with `tool_calls` field set); emit
     `ServerEvent::Entry`; advance `leaf_id`.
  3. Push the assistant message into `messages`.
  4. For each call: run tool-call hooks (emit `Error` on block, increment
     `consecutive_failures`, skip); emit `ToolStart`; call `execute_tool`;
     emit `ToolResult`; persist `BashExec` entry for bash calls; push tool
     result `Message` into `messages`.
  5. Apply guards: if `consecutive_failures >= 3`, emit `Error { fatal: false
     }`, return `Idle`. If `tool_calls_this_turn >= max_per_turn`, emit
     `Error`, return `Idle`.
  6. Increment `tool_call_round`. If `tool_call_round >= max_per_turn`, emit
     `Error`, return `Idle`.
  7. Write next `PipeOut::Llm(LlmRequest)` to stdout with updated `messages`.
  8. Return `Streaming { ..., response_text: String::new(), in_thinking: false,
     tool_calls_buf: vec![], finish_reason: None }` (all other fields
     preserved).
- `"stop"` | `"length"` (or unknown):
  1. Emit `ServerEvent::Done { status: reason }`.
  2. If `response_text` non-empty: persist assistant `Message` entry, emit
     `Entry`, advance `leaf_id`.
  3. Update tree meta (`leaf_id`, `updated_at`) via `store.save_tree_meta`.
  4. Return `Idle`.

**`cancel_turn`** — called on `Cmd(Stop)` regardless of state:

- If `Streaming` and `response_text` non-empty: persist partial assistant
  message, emit `Entry`.
- Emit `ServerEvent::Done { status: "cancelled" }`.
- Reset stop flag to `false` (same logic as current: a cancel arriving during
  idle must not poison the next turn).
- Return `Idle`.

**No `Arc<Mutex<>>`** on stdin/stdout. Both are owned by the single thread.
The `stop: Arc<AtomicBool>` is still needed because `execute_tool` passes it
into tool implementations so long-running bash commands can be interrupted.

**Helper functions to copy verbatim from `agent-core/src/agent.rs`** (private
to the worker crate):
`build_system_prompt`, `execute_tool`, `format_tool_output`,
`preview_tool_output`, `split_thinking_chunks`, `ThinkingSegment`,
`write_session_end`, `write_message_entry`, `resolve_repo_path`,
`truncate_for_log`.

Copy the `// INTENTIONAL:` comments from `SyncStdinChunkReader::drop` into a
comment on the `[DONE]`-skip branch in `process_chunk`, explaining why we
ignore `[DONE]` and rely on `Llm(Done)` instead.

---

#### `agent-core/src/agent.rs` — strip to library surface

**Remove:** `run_agent`, `build_system_prompt`, `execute_tool`,
`format_tool_output`, `preview_tool_output`, `split_thinking_chunks`,
`ThinkingSegment`, `write_session_end`, `write_message_entry`,
`resolve_repo_path`, `truncate_for_log`.

**Keep:** `build_context`, `auto_title`, `estimate_tokens`,
`estimate_context_tokens`.

**Keep** all existing `#[cfg(test)]` tests — they all exercise the kept
functions (`build_context`, `estimate_*`, `split_thinking_chunks` tests will
move to the worker crate alongside that function).

Update the module-level doc comment to reflect the reduced scope.

---

#### `agent-core/src/provider.rs` — remove pipe infrastructure

**Remove:** `LlmProvider` trait, `SyncPipeProvider`, `SyncStdinChunkReader`,
`StdinHandle`, `StdoutHandle` type aliases.

**Remove** the `impl LlmProvider for Provider` block. Move `stream_chat` into
`impl Provider` as an inherent method. Signature unchanged:
`pub fn stream_chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatStream>`.

Remove now-unused import:
`use crate::rpc::{LlmRequest, LlmResponse, PipeIn, PipeOut, WsCommand}`.

The `Provider` struct, `ChatResponse`, `generate_continuation_brief`, and all
existing `#[cfg(test)]` tests are unchanged.

---

#### `agent-core/src/types.rs`

**Remove** `AgentInput`. It was only used by the `next_input` closure in
`agent-worker` and `run_agent`; both are gone.

`ToolCallBuilder`, `ChatChunk`, and `ChatStream` stay: `ToolCallBuilder` and
`ChatChunk` are used by the worker's `process_chunk`; `ChatStream` is used by
`Provider::stream_chat` on the server side.

---

#### `agent-server/src/lifecycle.rs`

Remove `LlmProvider` from the import:
```rust
// before
use agent_core::provider::{LlmProvider, Provider};
// after
use agent_core::provider::Provider;
```

No other changes. `handle_llm_request` uses `provider.stream_chat(...)` where
`provider: Provider`; calling an inherent method needs no trait in scope.

---

**Verify:**
```
cargo build
cargo test
```

No behaviour changes: the agent loop logic is identical, only its home and
structure differ.

---

### Server: single-threaded per-worker event loop

- [x] Add `nix` + `rustls` + `webpki-roots` to `agent-server/Cargo.toml`; remove `mio`
- [x] Add `WorkerMsg` enum and slim down `WorkerEntry` in `lifecycle.rs`
- [x] Write `agent-server/src/worker_loop.rs`: `PollHandler` trait, `WorkerCtx`, all handler types, `run_event_loop`
- [x] Rewrite `spawn_worker` to start a single event-loop thread
- [x] Remove `spawn_stdin_writer`, `spawn_stderr_demux`, `run_stdout_proxy`, `handle_llm_request`, `worker_subscribe`, `worker_send_command` from `lifecycle.rs`
- [x] Simplify `ws.rs`: HTTP upgrade → `WorkerMsg::NewClient` → return; delete `run_session`
- [x] Remove `stream_chat` + `ChatStream` from `agent-core/src/provider.rs` (now dead)
- [x] `cargo build && cargo test`

**Goal:** Replace 1+N threads per worker (keeper + 1 per WS connection + stdin
writer + stderr demux) with a single event-loop thread per worker using
`nix::poll`. LLM HTTP streaming becomes a `rustls`-backed fd in the same loop.
Eliminates the `mpsc` subscriber channels, the `Waker` mechanism, and all
per-WS-session threads.

---

#### Thread model

**Before (per worker):**
- Keeper/stdout-proxy thread (blocks until worker exits; keeps bwrap PDEATHSIG alive)
- Stdin writer thread (drains `mpsc::Receiver<String>`, blocking-writes to child stdin)
- Stderr demux thread (drains child stderr, logs)
- 1 thread per connected WS client (blocks in `mio::Poll`; holds `mpsc::Receiver<ServerEvent>` + `Waker`)
- Ad-hoc LLM threads (1 per LLM request; blocking HTTP via `ureq`)
- Ad-hoc auto-title thread (post-Done; off-loop, injects via `Waker`)

**After (per worker):**
- 1 event-loop thread (owns child stdio, all WS connections, LLM TLS state;
  also serves as the keeper thread so bwrap PDEATHSIG still works)
- Ad-hoc auto-title thread (unchanged; injects via `WorkerMsg::InjectEvent`)

---

#### Dep changes (`agent-server/Cargo.toml`)

Remove `mio`. Add:

```toml
nix          = { version = "0.29", features = ["poll", "fs"] }
rustls       = { version = "0.23", default-features = false, features = ["ring", "logging", "tls12"] }
webpki-roots = "0.26"
```

`tungstenite` stays (still used for WS frame parsing/writing).

---

#### `agent-server/src/lifecycle.rs` — changes

**`WorkerEntry` becomes:**

```rust
pub struct WorkerEntry {
    pub pid: u32,
    pub child: Option<Child>,
    pub msg_tx: mpsc::SyncSender<WorkerMsg>,
    pub notify_write: std::fs::File,  // write end of wakeup pipe; write one byte to unblock poll
}
```

**New enum:**

```rust
pub enum WorkerMsg {
    NewClient(Box<crate::worker_loop::WsClient>),
    InjectEvent(ServerEvent),  // from auto-title thread
    Stop,
}
```

**`spawn_worker`:** after spawning the subprocess (unchanged), build a
`nix::unistd::pipe()`, create a `mpsc::sync_channel(64)`, then spawn ONE
thread that calls `worker_loop::run_event_loop(...)`. The `WorkerEntry`
inserted into `ACTIVE_WORKERS` holds the `msg_tx` and the write end of the
wakeup pipe. The rendezvous pattern (sync_channel for spawn confirmation) is
unchanged.

**Remove entirely:** `spawn_stdin_writer`, `spawn_stderr_demux`,
`run_stdout_proxy`, `handle_llm_request`, `worker_subscribe`,
`worker_send_command`.

**`worker_stop`:** now sends `WorkerMsg::Stop` via `msg_tx` and writes one
byte to `notify_write` to unblock the poll. Called unchanged from
`shutdown_all`.

**`broadcast_meta_update`:** sends `WorkerMsg::InjectEvent(MetaUpdate {...})`
via `msg_tx` + writes one byte to `notify_write`.

---

#### `agent-server/src/worker_loop.rs` — new file

**`PollHandler` trait:**

```rust
pub trait PollHandler {
    fn fd(&self) -> RawFd;
    fn interests(&self) -> PollFlags;
    /// Return false to deregister (handler is dropped).
    fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool;
}
```

**`WorkerCtx`:**

```rust
pub struct WorkerCtx {
    pub tree_id: String,
    pub stdin: BufWriter<ChildStdin>,
    pub ws_clients: Vec<WsClient>,
    pub event_buffer: VecDeque<ServerEvent>,   // catch-up for new subscribers
    pub store: Arc<Store>,
    pub cfg: Arc<Config>,
    pub msg_rx: mpsc::Receiver<WorkerMsg>,
    pub tls_config: Arc<rustls::ClientConfig>,  // built once; reused per LLM request
    pub new_handlers: Vec<Box<dyn PollHandler>>, // handlers to add after on_ready returns
}
```

`WorkerCtx::broadcast(ev)` buffers `Entry` events in `event_buffer` (cap 1000,
pop front when full), then serializes and sends to all `ws_clients`. Clients
that fail to write are removed.

`WorkerCtx::stdin_send(json_line)` writes a line to `self.stdin` and flushes.

**`WsClient`** (owns one live WebSocket connection):

```rust
pub struct WsClient {
    ws: tungstenite::WebSocket<std::net::TcpStream>,
    last_ping: Instant,
    last_pong: Instant,
}
```

`WsClient::write_event(ev)` serializes the event and calls `ws.send(Text(...))`.

`WsClient::on_readable(stdin: &mut BufWriter<ChildStdin>) -> bool`:
- `ws.read()` → `Text(s)` → deserialize as `WsCommand`, serialize as
  `PipeIn::Cmd(cmd)`, write to `stdin`
- `Pong(_)` → update `last_pong`
- `Close(_)` / any error except `WouldBlock` → return `false`
- `WouldBlock` → return `true`

`WsClient::tick(stdin: &mut BufWriter<ChildStdin>) -> bool`: send keepalive
pings every 30s; return `false` if pong has not arrived within 90s.

**`StdoutHandler`** owns a `BufReader<ChildStdout>`:

`on_ready`: read one line; parse as `PipeOut`. On `PipeOut::Event(ev)`: call
`ctx.broadcast(ev)`; if the event is `Done { .. }`, check if tree needs a
title and if so spawn an auto-title thread (same logic as current
`run_stdout_proxy`) — the thread gets a clone of `msg_tx` + `notify_write` and
sends `WorkerMsg::InjectEvent(MetaUpdate {...})` when done. On
`PipeOut::Llm(req)`: build `LlmHandler::new(req, &ctx.cfg, ctx.tls_config.clone())`
(synchronous TCP connect + set nonblocking) and push to `ctx.new_handlers`.
Return `false` only on EOF/error (signals event loop to exit).

**`StderrHandler`** owns a `BufReader<ChildStderr>`:

`on_ready`: read one line, log it, append to a `VecDeque<String>` cap 20.
On EOF, stores the buffer so the post-exit crash-detection (currently in
`run_stdout_proxy`) can use it. Returns `false` on EOF.

After the loop exits (stdout closed), the crash detection and `ACTIVE_WORKERS`
removal logic (currently at the end of `run_stdout_proxy`) runs in the event
loop function body, not inside a handler.

**`NotifyHandler`** owns the read end of the wakeup pipe and a reference to
`mpsc::Receiver<WorkerMsg>`:

`on_ready`: drain the pipe (read until `EAGAIN`); then drain `ctx.msg_rx`:
- `NewClient(ws_client)`: send catch-up snapshot from `ctx.event_buffer`, then
  push to `ctx.ws_clients`
- `InjectEvent(ev)`: call `ctx.broadcast(ev)`
- `Stop`: write `PipeIn::Cmd(WsCommand::Stop)` to `ctx.stdin`

Returns `true` always (stays registered).

**`LlmHandler`** drives a single HTTPS streaming request:

```rust
pub struct LlmHandler {
    tcp: std::net::TcpStream,    // set non-blocking after connect
    tls: rustls::ClientConnection,
    state: LlmState,
    req_id: u64,
    line_buf: String,            // partial SSE line accumulator (Streaming state)
}

enum LlmState {
    TlsHandshake,
    SendRequest { body: Vec<u8>, sent: usize },
    ReadHeaders  { buf: Vec<u8> },
    Streaming,
}
```

`LlmHandler::new(req, cfg, tls_config)`:
1. Parse `cfg.provider.base_url` to extract host, port (default 443), path.
2. `TcpStream::connect((host, port))` — synchronous; fine here because
   connect time (<100ms) is negligible vs. LLM turn latency, and each worker
   has its own thread.
3. `tcp.set_nonblocking(true)`.
4. Build `rustls::ClientConnection::new(tls_config, ServerName::try_from(host))`.
5. Serialize the HTTP POST request (headers + JSON body from `req`) into `body`.
6. Return handler in `TlsHandshake` state.

`interests()`: returns `POLLIN` if `tls.wants_read()`, `POLLOUT` if
`tls.wants_write()` (covers both handshake and data phases transparently).

`on_ready`:
- **Any state**: call `tls.read_tls(&mut tcp)` if `POLLIN` was ready;
  call `tls.write_tls(&mut tcp)` if `POLLOUT` was ready;
  call `tls.process_new_packets()` after reads.
- **`TlsHandshake`**: if `!tls.is_handshaking()`, advance to `SendRequest`.
- **`SendRequest`**: write `body[sent..]` into `tls.writer()`, update `sent`.
  When `sent == body.len()`, advance to `ReadHeaders { buf: Vec::new() }`.
- **`ReadHeaders`**: read from `tls.reader()` into `buf`; use `httparse` to
  find the header/body boundary. On non-200 status, send
  `PipeIn::Llm(LlmResponse::Error)` to `ctx.stdin` and return `false`.
  On success advance to `Streaming`.
- **`Streaming`**: read from `tls.reader()` into `line_buf`; emit each
  complete `\n`-terminated line as `PipeIn::Llm(LlmResponse::Chunk { id:
  req_id, data: line })` to `ctx.stdin`. On EOF/error/`[DONE]`, send
  `PipeIn::Llm(LlmResponse::Done { id: req_id })` and return `false`.

**`run_event_loop`:**

```rust
pub fn run_event_loop(
    tree_id: String,
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
    child_stderr: ChildStderr,
    msg_rx: mpsc::Receiver<WorkerMsg>,
    notify_read: std::fs::File,
    notify_write: std::fs::File,   // kept alive to prevent EOF on read end
    store: Arc<Store>,
    cfg: Arc<Config>,
    stderr_buf: Arc<Mutex<VecDeque<String>>>,  // shared with WorkerEntry for crash reporting
    spawn_tx: mpsc::SyncSender<Result<(), String>>,
    child: Child,
)
```

1. Build `tls_config` (load `webpki_roots::TLS_SERVER_ROOTS`; construct
   `ClientConfig`; wrap in `Arc`).
2. Signal `spawn_tx.send(Ok(()))` (unblocks `spawn_worker`).
3. Build `WorkerCtx`, initial handler list:
   `[StdoutHandler, StderrHandler, NotifyHandler]`.
4. Main loop:
   ```
   loop {
       // Build pollfds from handlers + ws_clients
       nix::poll::poll(&mut pollfds, 30_000ms timeout)?;
       // Dispatch ready handlers (collect indices first, swap_remove on false)
       // Dispatch ready ws_clients (on_readable; remove on false)
       // Append ctx.new_handlers to handlers
       // Tick all ws_clients (keepalive); remove timed-out ones
       // Break when StdoutHandler returns false (EOF)
   }
   ```
5. After loop: child.wait(), check exit status, emit crash events if needed
   (same logic as current `run_stdout_proxy` post-loop). Remove from
   `ACTIVE_WORKERS`.

---

#### `agent-server/src/ws.rs` — simplify

`accept()` keeps everything up to and including the WS handshake and the
catch-up snapshot send. After upgrade:
- Look up `WorkerEntry` from `ACTIVE_WORKERS`
- Call `stream.set_nonblocking(true)`
- Box the `WsClient`, send as `WorkerMsg::NewClient`
- Write one byte to `entry.notify_write`
- Return (the per-connection thread exits immediately)

Delete `run_session` entirely.

---

#### `agent-core/src/provider.rs` — remove dead code

Remove `stream_chat` and `ChatStream` (both are dead after this step; no
callers remain in agent-core or agent-server).

---

**Verify:**

```
cargo build
cargo test
# manual smoke: start server, open two WS connections to the same tree,
# send a message, confirm both receive streaming events
```

---

### Provider normalization

- [ ] Define `LlmBackend` trait in `agent-core/src/provider.rs`
- [ ] Normalize `ChatChunk` as the canonical cross-provider chunk type
- [ ] Implement `OpenAiBackend` (wraps current `Provider` logic)
- [ ] Implement `AnthropicBackend`
- [ ] Update `handle_llm_request` to use `Box<dyn LlmBackend>`
- [ ] Update pipe protocol: `Chunk.data` carries normalized `ChatChunk` JSON
- [ ] Update `process_chunk` in worker to deserialize `ChatChunk` directly

**Goal:** Make the worker fully provider-agnostic. Today `process_chunk`
implicitly assumes OpenAI SSE wire format (strips `"data: "` prefix, checks
for `"[DONE]"`, parses OpenAI-shaped JSON). The fix is to normalize at the
server boundary: each provider adapter translates its own wire format into
`ChatChunk` before the chunk reaches the pipe. The worker then deserializes
`ChatChunk` directly — no SSE parsing, no `[DONE]` handling, no
provider-specific field names.

**`LlmBackend` trait** (lives in `agent-core/src/provider.rs`, server-side
contract only — worker never sees it):

```rust
pub trait LlmBackend {
    fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<Box<dyn Iterator<Item = Result<ChatChunk>>>>;
}
```

**`ChatChunk` normalization:** audit the struct against Anthropic's streaming
format. The current fields (`choices[0].delta.content`,
`choices[0].delta.tool_calls`, `choices[0].delta.reasoning`,
`choices[0].finish_reason`, `usage`) are OpenAI-shaped. Options:

- Flatten to a provider-neutral shape (e.g. `delta_text`, `delta_reasoning`,
  `tool_call_delta`, `finish_reason`, `usage`) — cleaner long-term but
  breaks existing serde.
- Keep the OpenAI shape and have the Anthropic adapter map into it — simpler
  short-term. Anthropic's `content_block_delta` events map reasonably to
  `choices[0].delta`.

Recommend the flat approach since `ChatChunk` is currently only
deserialized in `process_chunk` (worker-internal after the refactor) so
there is no external serde compatibility to preserve.

**Pipe protocol change:** `LlmResponse::Chunk { data: String }` currently
carries a raw SSE line. After this step it carries
`serde_json::to_string(&chunk)` where `chunk: ChatChunk`. The `[DONE]`
sentinel and `"data: "` prefix disappear from the protocol entirely.
`process_chunk` becomes:

```rust
let chunk: ChatChunk = serde_json::from_str(&data)?;
// use chunk fields directly — no SSE parsing
```

**Config:** add a `provider.kind` field (`"openai"` | `"anthropic"`, default
`"openai"`). `handle_llm_request` constructs the right `Box<dyn LlmBackend>`
from config. The existing `Provider` struct becomes `OpenAiBackend`; its
`stream_chat` inherent method becomes the trait impl.

**Note:** do this step after the state machine refactor, since
`process_chunk` is being rewritten there anyway. Doing both together avoids
writing SSE parsing twice.

---
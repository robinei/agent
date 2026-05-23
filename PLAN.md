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
- **Error style:** `Result<T, String>` for `tree_io` and server/CLI call
  sites; `thiserror` enum (`StoreError`) for `Store` in `agent-worker`
  (matches existing pattern).
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
- **`tree_io` lives in `agent-core`** — both the worker's `Store` and the
  server use it for `meta.json` operations. This avoids duplicating
  serialization, directory scanning, and atomic-write logic. `Store` wraps
  `tree_io` for meta access; the server and CLI use `tree_io` directly.

---

## Architectural invariants (after all steps complete)

- **Only the worker knows about `data.jsonl`.** `Store` (in `agent-worker`)
  owns all `data.jsonl` I/O: the header, all `Entry` records, `read_all_entries`.
  The server and CLI never open `data.jsonl`.
- **The server and CLI use `tree_io` directly for `meta.json`.**
  `tree_io` functions all accept `base: &Path` so they are testable with
  temp dirs and have no hardcoded global state.
- **Clients receive entries exclusively over WebSocket.** The REST
  `/entries` endpoint is gone; `WsCommand::GetEntries` asks the worker to
  replay history to a new client.
- **Auto-title runs inside the worker's state machine.**
  No background threads on the server side. When `Done` fires, the server
  sends `WsCommand::AutoTitle`; the worker adds an `AutoTitling` state,
  uses the existing `PipeOut::Llm` → `PipeIn::Llm` path (no direct
  network from the worker, which may be sandboxed), saves the title, emits
  `MetaUpdate`.

---

## Step template

```
### <Name>

- [ ] todo / - [x] done

**Goal:** one or two sentences.

**Spec:** file paths, signatures, tests, do-not-modify list.

**Verify:** commands that prove it works.
```

On completion: delete this entry, then commit code + PLAN.md together with:

```
<crate/area>: <brief title>

<what was built, 1-2 sentences>

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
```

---

## Pending Steps

### Step 1 — Add `tree_io` to `agent-core`; refactor `Store` to delegate

- [ ] todo

**Goal:** A shared `tree_io` module in `agent-core` provides all `meta.json`
read/write/list helpers, each accepting `base: &Path` so they are fully
testable with temp dirs. `Store` (still in `agent-core` for this step) is
refactored internally to delegate its meta.json operations to `tree_io`.
No crate-boundary changes; all imports and public APIs remain the same.

**Spec:**

**New file `agent-core/src/tree_io.rs`:**

```rust
use std::path::{Path, PathBuf};
use crate::types::{TreeHeader, TreeMeta};

/// Returns ~/.agent/trees/{id} (or base/trees/{id} in tests).
pub fn tree_dir(base: &Path, tree_id: &str) -> PathBuf

/// Read and parse meta.json. Returns None if the file doesn't exist.
pub fn read_meta(base: &Path, tree_id: &str) -> Result<Option<TreeMeta>, String>

/// Write meta.json atomically (write to .tmp, rename over target).
/// Creates the tree directory if it doesn't exist.
pub fn write_meta(base: &Path, meta: &TreeMeta) -> Result<(), String>

/// Scan base/trees/*/meta.json and return all parseable TreeMetas,
/// sorted by updated_at descending. Logs and skips corrupt files.
pub fn list_trees(base: &Path) -> Result<Vec<TreeMeta>, String>

/// Create a new tree: mkdir base/trees/{id}, write the data.jsonl header
/// line, write meta.json atomically.
pub fn create_tree(base: &Path, meta: &TreeMeta) -> Result<(), String>
```

All five functions return `Result<_, String>` — no `thiserror` in `tree_io`
itself. The `data.jsonl` header written by `create_tree` is:
```json
{"kind":"meta","version":1,"id":"<tree_id>"}
```
(matching the existing `TreeHeader` serialisation).

`agent-core/src/lib.rs`:
- Add `pub mod tree_io;`
- Keep `pub mod store;` (moved in Step 2)

**Refactor `agent-core/src/store.rs`** — delegate meta.json ops to `tree_io`:
- `load_tree_meta(&self, id)` → `tree_io::read_meta(&self.base_dir, id)`
  (convert `String` error to `StoreError::Io` via `std::io::Error::other`)
- `save_tree_meta(&self, meta)` → `tree_io::write_meta(&self.base_dir, meta)`
  then `self.update_index_cache(meta)` (same as now)
- `tree_dir_for(&self, id)` → `tree_io::tree_dir(&self.base_dir, id)`
- `rebuild_index(&self)` → `tree_io::list_trees(&self.base_dir)`, then
  populate `index_cache` from the result and write `index.json` as before
- `create_tree_file` — keep its own implementation (writes the JSONL header
  directly; it does not call `tree_io::create_tree` to avoid double-writing
  meta.json since the caller writes meta separately)
- All `data.jsonl` methods (`append_entry`, `read_all_entries`, `jsonl_path`)
  remain unchanged

`agent-core/Cargo.toml`: no changes (thiserror still needed by `StoreError`).

`tree_io` tests (in `agent-core/src/tree_io.rs`):
```rust
#[cfg(test)]
mod tests {
    // create_tree + read_meta roundtrip
    // write_meta atomicity (check .tmp is gone)
    // list_trees sorts by updated_at desc, skips corrupt files
}
```
Use `tempfile::TempDir` for isolation (already a dev-dep of `agent-core`).

**Do not modify:**
- `agent-core/src/types.rs`, `config.rs`, `rpc.rs`, `util.rs`
- `agent-worker/`, `agent-server/`, `agent-cli/` — no changes in this step

**Verify:**
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
```

---

### Step 2 — Move `Store` to `agent-worker`; server and CLI use `tree_io` directly

- [ ] todo

**Goal:** After this step, `Store` (and all `data.jsonl` knowledge) lives
exclusively in `agent-worker`. The server uses `tree_io` from `agent-core`
for all `meta.json` operations. The CLI's embedded path uses `tree_io`
directly. `agent-server/Cargo.toml` drops the `agent-worker` dependency.
The CLI drops its `Store` dependency entirely.

**Spec:**

**New file `agent-worker/src/store.rs`** — copy `agent-core/src/store.rs` verbatim, then:
- Change `use crate::config::agent_dir` → `use agent_core::config::agent_dir`
- Change `use crate::types::...` → `use agent_core::types::...`
- The `tree_io` delegation added in Step 1 stays as-is; `agent_core::tree_io`
  is reachable because `agent-worker` already depends on `agent-core`

`agent-worker/src/lib.rs`:
- Add `pub mod store;`
- Change `use agent_core::store::Store` → `use crate::store::Store`

`agent-worker/Cargo.toml`:
- Add `thiserror = "1"` (used by `StoreError` in the moved `store.rs`)

Worker files that import Store:
- `agent-worker/src/turn.rs`: `use agent_core::store::Store` → `use crate::store::Store`
- `agent-worker/src/util.rs`: same
- `agent-worker/src/tools/search.rs`: same (in `#[cfg(test)]`)

**`agent-core/src/store.rs`** — delete.
`agent-core/src/lib.rs` — remove `pub mod store;`.
`agent-core/Cargo.toml` — remove `thiserror` dep (no longer used).

**`agent-server` — remove `Store`; switch to `tree_io`:**

`agent-server/Cargo.toml`:
- Remove `agent-worker = { path = "../agent-worker" }`

`agent-server/src/lib.rs`:
- Remove `use agent_core::store::Store` (and `use agent_worker::store::Store`)
- `embed_init(config, to_stderr)` — drop `store` param; replace
  `store.rebuild_index()` with `agent_core::tree_io::list_trees(&agent_core::config::agent_dir())`
- `serve(config, shutdown)` — drop `store` param; update `shutdown_all` call
  (Step 2 removes its `store` param too, see lifecycle below)
- `run()` — remove `Store::default()` construction, update `embed_init` /
  `serve` calls

`agent-server/src/http.rs`:
- `handle_connection(stream, cfg)` — drop `store` param
- Remove `use agent_worker::store::Store` import

`agent-server/src/ws.rs`:
- `accept(...)` — drop `store` param if present; propagate through callers

`agent-server/src/routes.rs`:
- Remove `use agent_worker::store::Store` and `use crate::auto_title`
- `dispatch(method, path, body, cfg)` — drop `store` param
- `handle_list_trees()` →
  `agent_core::tree_io::list_trees(&agent_core::config::agent_dir())`
- `handle_create_tree(body, cfg)` →
  build `TreeMeta`, call `agent_core::tree_io::create_tree(&agent_dir(), &meta)`
  (this writes dir + data.jsonl header + meta.json atomically)
- `handle_get_tree(id)` →
  `agent_core::tree_io::read_meta(&agent_dir(), id)`
- `handle_update_tree(id, body)` →
  `tree_io::read_meta` then `tree_io::write_meta`
- **Delete `handle_list_entries()`** entirely
- **Delete `/trees/{id}/entries` route** from dispatch
- **Delete `handle_auto_title()`** and the `("POST", "auto-title")` route
  (auto-title is worker-driven after Step 4; no REST trigger needed)
- Tests: use `tempfile::TempDir`, set `agent_dir` via env override or call
  `tree_io` functions directly with the temp path

`agent-server/src/lifecycle.rs`:
- `spawn_worker(tree_id, cfg)` — drop `store` param; load meta via
  `agent_core::tree_io::read_meta(&agent_dir(), tree_id)`
- `spawn_auto_title(ctx)` — **delete** (replaced in Step 4)
- `shutdown_all()` — drop `_store` param (was already unused)
- Remove all Store imports

`agent-server/src/worker_ctx.rs`:
- Remove `pub store: Arc<Store>` field from `WorkerCtx`
- Remove Store import

`agent-server/src/worker_loop.rs`:
- Remove Store import (it was only passed through, not used by the loop itself)

`agent-server/src/handlers.rs`:
- Remove any Store import; the `spawn_auto_title(ctx)` call stays for now
  (deleted in Step 4)

**Delete `agent-server/src/auto_title.rs`** (it imports both `Store` and
`agent_worker::agent::build_context`, neither of which the server will have
access to; auto-title moves to the worker in Step 4).
`agent-server/src/lib.rs` — remove `pub mod auto_title;`.

**`agent-cli` — drop `Store` entirely:**

`agent-cli/src/lib.rs`:
- Remove `Arc::new(agent_core::store::Store::default())` from `resolve_backend`
- `embed_init(config.clone(), false)` — no store arg
- `serve(cc, no_shutdown)` — no store arg
- `embedded_session(tree_id, config)` — drop `store` param; update call to
  `agent_server::http::handle_connection(stream, cfg)` (no store)
- Remove `use agent_core::store::Store`
- `Backend::auto_title()` — keep for now (removed in Step 4)

`agent-cli/src/local.rs`:
- Remove `use agent_core::store::Store`; `LocalClient` no longer holds `store`
- `LocalClient { config: Arc<Config> }` (store field removed)
- `LocalClient::new(config)` — no store param
- `list_trees()` →
  `agent_core::tree_io::list_trees(&agent_core::config::agent_dir())`
- `create_tree(...)` → build `TreeMeta`, call
  `agent_core::tree_io::create_tree(&agent_dir(), &meta)`.
  **Remove the `SessionStart` and `ModelSet` entry writes** — those are the
  worker's responsibility (it writes them in `startup_writes`).
- `get_tree(id)` →
  `agent_core::tree_io::read_meta(&agent_dir(), id)`
- `get_entries()` — keep stub for now (removed in Step 3)
- `stop_agent()` — unchanged (`lifecycle::worker_stop`)
- `auto_title()` — keep stub for now (removed in Step 4)

**Do not modify:**
- `agent-core/src/tree_io.rs` (done in Step 1)
- `agent-core/src/types.rs`, `config.rs`, `rpc.rs`
- `agent-worker/src/store.rs` internal `data.jsonl` methods

**Verify:**
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
# Smoke: create a tree and connect; entries should still flow over WS
```

---

### Step 3 — WS history push; remove `/entries` REST route and `event_buffer`

- [ ] todo

**Goal:** Remove the CLI's `/entries n` command, the `Backend::get_entries()`
method, and the server's `event_buffer` replay. Replace with
`WsCommand::GetEntries`: when a new WS client connects, the server sends this
command and the worker pushes all past entries as `Entry` events over the
normal broadcast path.

**Spec:**

`agent-core/src/rpc.rs` — add `GetEntries` variant to `WsCommand`:

```rust
#[serde(tag = "method", rename_all = "snake_case")]
pub enum WsCommand {
    Message { params: MessageParams },
    Stop,
    GetEntries { count: Option<usize> },  // None = all
}
```

`agent-worker/src/lib.rs` — handle `WsCommand::GetEntries` in
`dispatch_pipe_in`:

```rust
PipeIn::Cmd(WsCommand::GetEntries { count }) => {
    let entries = store.read_all_entries(tree_id).unwrap_or_default();
    let to_emit: &[Entry] = if let Some(n) = count {
        let len = entries.len();
        &entries[len.saturating_sub(n)..]
    } else {
        &entries
    };
    for entry in to_emit {
        emit_event(out, ServerEvent::Entry(entry.clone()));
    }
    out.flush().ok();
}
```

`agent-server/src/handlers.rs` — `NotifyHandler::on_ready`, `NewClient` arm:

```rust
Ok(WorkerMsg::NewClient(mut ws_client)) => {
    // Add the client first so it receives the GetEntries response.
    ctx.ws_clients.push(*ws_client);
    ctx.send_pipe_in(&PipeIn::Cmd(WsCommand::GetEntries { count: None }));
}
```

`agent-server/src/worker_ctx.rs`:
- Remove `event_buffer: VecDeque<ServerEvent>` field
- Remove the `if matches!(ev, ServerEvent::Entry(_)) { ... }` buffer block
  from `broadcast()`
- Remove `use std::collections::VecDeque` if no longer needed

`agent-cli/src/interactive.rs`:
- Remove `CliCommand::Entries(Option<usize>)` variant
- Remove `/entries` branch from `parse_input()`
- Remove the `CliCommand::Entries(n)` match arm in the dispatch loop
- Remove `replay_entries()` function entirely
- On reconnect: remove the `get_entries` call; Entry events now arrive
  automatically via the WS push triggered by `NewClient`

`agent-cli/src/lib.rs`:
- Remove `get_entries()` from the `Backend` enum

`agent-cli/src/client.rs`:
- Remove `get_entries()` method
- Remove `entries_url()` helper
- Remove `test_get_entries_url` test (if present)

`agent-cli/src/local.rs`:
- Remove `get_entries()` method

**Do not modify:**
- `agent-worker/src/store.rs` — `read_all_entries` stays, used by GetEntries

**Verify:**
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
# Manual: `agent cli --repo-path .` → start, check /entries absent from help
#   disconnect and reconnect → past entries appear via WS push
```

---

### Step 4 — Auto-title moves into worker state machine

- [ ] todo

**Goal:** Auto-titling runs inside the worker's event loop as a new
`AgentState::AutoTitling` state. When `Done` fires, the server sends
`WsCommand::AutoTitle` to the worker. The worker reads its entries, sends
an LLM request via the existing `PipeOut::Llm` channel (the worker has no
guaranteed network — the server does the HTTP call as always), accumulates
the streaming response, saves the title, and emits `MetaUpdate`. No
background threads on the server. No `auto_title.rs` on the server.

**Spec:**

`agent-core/src/rpc.rs` — add `AutoTitle` variant to `WsCommand`:

```rust
pub enum WsCommand {
    Message { params: MessageParams },
    Stop,
    GetEntries { count: Option<usize> },
    AutoTitle,
}
```

`agent-worker/src/lib.rs` — add `AutoTitling` to `AgentState`:

```rust
pub(crate) enum AgentState {
    Idle,
    Streaming { /* unchanged */ },
    AutoTitling {
        req_id: u64,
        accumulated: String,
    },
}
```

Handle `WsCommand::AutoTitle` in `dispatch_pipe_in`:

```rust
PipeIn::Cmd(WsCommand::AutoTitle) => {
    if !matches!(state, AgentState::Idle) { return; }
    let meta = match store.get_tree(tree_id).ok().flatten() {
        Some(m) => m,
        None => return,
    };
    if meta.title.is_some() { return; }
    let entries = store.read_all_entries(tree_id).unwrap_or_default();
    let leaf_id = match &meta.leaf_id {
        Some(id) => id.clone(),
        None => return,
    };
    let mut messages = crate::agent::build_context(&entries, &leaf_id);
    messages.insert(0, Message {
        role: MessageRole::System,
        content: MessageContent::Text(
            "Generate a concise title (6 words or fewer) for this coding \
             conversation. Return ONLY the title text, no quotes, no \
             punctuation, no explanation.".into()
        ),
        ..Default::default()
    });
    *req_id += 1;
    let llm_req = agent_core::rpc::LlmRequest { id: *req_id, messages, tools: vec![] };
    agent_core::rpc::write_json_line(out, &agent_core::rpc::PipeOut::Llm(llm_req))
        .ok();
    out.flush().ok();
    *state = AgentState::AutoTitling { req_id: *req_id, accumulated: String::new() };
}
```

Extend the `PipeIn::Llm(Chunk)` / `Done` / `Error` arms to handle
`AutoTitling`:

```rust
PipeIn::Llm(LlmResponse::Chunk { id, data, .. }) => {
    if id != *req_id { return; }
    match state {
        AgentState::Streaming { .. } => process_chunk(&data, state, out),
        AgentState::AutoTitling { ref mut accumulated, .. } => {
            // Parse data (same JSON shape as streaming chunks).
            // Extract delta_text and append to accumulated.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(t) = v["delta_text"].as_str() {
                    accumulated.push_str(t);
                }
            }
        }
        _ => {}
    }
}

PipeIn::Llm(LlmResponse::Done { id, .. }) => {
    if id != *req_id { return; }
    match state {
        AgentState::Streaming { .. } => { /* existing finish_response path */ }
        AgentState::AutoTitling { ref accumulated, .. } => {
            let title = accumulated.trim().trim_matches('"').to_string();
            if !title.is_empty() {
                if let Ok(Some(mut meta)) = store.get_tree(tree_id) {
                    meta.title = Some(title.clone());
                    let _ = store.save_tree_meta(&meta);
                }
                emit_event(out, ServerEvent::MetaUpdate { title: Some(title) });
                out.flush().ok();
            }
            *state = AgentState::Idle;
        }
        _ => {}
    }
}

PipeIn::Llm(LlmResponse::Error { id, message, .. }) => {
    if id != *req_id { return; }
    match state {
        AgentState::Streaming { .. } => { /* existing path */ }
        AgentState::AutoTitling { .. } => {
            log::warn!("[worker] auto-title LLM error: {}", message);
            *state = AgentState::Idle;
        }
        _ => {}
    }
}
```

`PipeIn::Cmd(WsCommand::Message)` while `AutoTitling` → log a warning and
drop (auto-titling completes in one round-trip; the user can resend).

`agent-server/src/handlers.rs` — `StdoutHandler::on_ready`:

```rust
if matches!(event, ServerEvent::Done { .. }) {
    ctx.send_pipe_in(&PipeIn::Cmd(WsCommand::AutoTitle));
}
ctx.broadcast(event);
```

Remove the `spawn_auto_title(ctx)` call. No other changes to handlers.rs.

`agent-server/src/lifecycle.rs`:
- Delete `spawn_auto_title()` (already stub-deleted if Step 2 removed it;
  if not, delete here)

`agent-cli/src/lib.rs`:
- Remove `Backend::auto_title()` from the enum and both match arms
- `session_and_stream()`: remove the `backend.auto_title(&meta.id)` call;
  instead, listen for `ServerEvent::MetaUpdate { title }` in the existing
  WS event loop and print the title when received

`agent-cli/src/client.rs`:
- Remove `auto_title()` method and `auto_title_url()` helper

`agent-cli/src/local.rs`:
- Remove `auto_title()` method

**Do not modify:**
- `agent-worker/src/agent.rs` — `build_context` is called directly, unchanged
- `agent-core/src/rpc.rs` beyond the `AutoTitle` variant addition

**Verify:**
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
# Manual: send message to new tree, wait for Done,
#   verify MetaUpdate arrives over WS with a non-empty title
```

---

### Summary of files to create / delete

**New files:**
- `agent-core/src/tree_io.rs` (Step 1)
- `agent-worker/src/store.rs` (Step 2)

**Deleted files:**
- `agent-core/src/store.rs` (Step 2)
- `agent-server/src/auto_title.rs` (Step 2)

### Files modified across steps

| File | Step(s) |
|------|---------|
| `agent-core/src/lib.rs` | 1, 2 |
| `agent-core/src/tree_io.rs` | 1 (new) |
| `agent-core/src/store.rs` | 1 (refactor), 2 (delete) |
| `agent-core/Cargo.toml` | 2 |
| `agent-core/src/rpc.rs` | 3, 4 |
| `agent-worker/src/lib.rs` | 2, 3, 4 |
| `agent-worker/src/store.rs` | 2 (new) |
| `agent-worker/src/turn.rs` | 2 |
| `agent-worker/src/util.rs` | 2 |
| `agent-worker/src/tools/search.rs` | 2 (test import) |
| `agent-worker/Cargo.toml` | 2 |
| `agent-server/src/lib.rs` | 2, 4 |
| `agent-server/src/routes.rs` | 2 |
| `agent-server/src/http.rs` | 2 |
| `agent-server/src/ws.rs` | 2 |
| `agent-server/src/lifecycle.rs` | 2, 4 |
| `agent-server/src/worker_ctx.rs` | 2, 3 |
| `agent-server/src/worker_loop.rs` | 2 |
| `agent-server/src/handlers.rs` | 3, 4 |
| `agent-server/src/auto_title.rs` | 2 (delete) |
| `agent-server/Cargo.toml` | 2 |
| `agent-cli/src/lib.rs` | 2, 3, 4 |
| `agent-cli/src/interactive.rs` | 3 |
| `agent-cli/src/client.rs` | 3, 4 |
| `agent-cli/src/local.rs` | 2, 3, 4 |

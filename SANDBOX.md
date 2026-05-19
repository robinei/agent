# Per-Tree Sandboxed Workers

Replace the current thread-per-agent model with **process-per-agent**, where each
worker runs inside a bubblewrap sandbox configured from a **per-tree privilege
envelope**. Replace SSE streaming with a **bidirectional WebSocket** session channel.

---

## Motivation

The agent runs `bash` and other filesystem tools on behalf of an LLM. The LLM
will occasionally do something stupid — `rm -rf $HOME`, `cat ~/.aws/credentials
| llm`, write to the wrong directory. We want to cap the blast radius of that
stupidity cheaply, without permission prompts or runtime ceremony.

The mechanism is a **per-tree privilege envelope**: each tree carries a small
sandbox config (writable paths, network override, credential overrides), and
the worker for that tree runs inside a bwrap configured from it. Default-deny
on writes (just the repo path), default-allow on reads, default-allow on net,
default-hide on credential directories.

Thread-per-agent can't do this — `bash` inherits the process's filesystem view.
Process-per-agent + bwrap is the only way to give each tree a tailored envelope.

This is **not** designed against an adversarial human, only against careless
model output. Inter-tree isolation is a side benefit, not the goal.

WebSocket replaces SSE because:
- SSE forced an awkward `rouille::Upgrade` workaround (see `DEBUG.md`)
- Browsers and `tungstenite` both speak WS natively
- One channel handles both commands and events; no separate POST + GET pair

We also drop `rouille` entirely while we're touching this. The HTTP
surface is seven endpoints, all JSON request/response — small enough
that a hand-rolled HTTP layer on `std::net::TcpListener` + `httparse`
is shorter than wrestling with a framework that's effectively
unmaintained. Three benefits beyond dependency count:

1. **Owning the `TcpStream` directly** means WS upgrade is clean: we
   set the socket non-blocking before handing it to `tungstenite`,
   which makes the single-threaded read+write loop trivial (no
   `Mutex<WebSocket>` deadlock, no dual-port workaround).
2. **One port for everything** — HTTP and WS share the listener, dispatched
   by inspecting the `Upgrade` header.
3. **Long-term stable surface area** — `httparse` and `tungstenite` are
   both small, well-maintained, and unlikely to churn.

---

## Architecture Overview

```
┌──────────────────┐           ┌─────────────────────────────────────────────┐
│  agent cli / PWA │──── WS ──▶│  agent server (HTTP + WebSocket)            │
└──────────────────┘           │                                             │
                               │  tree CRUD       →  HTTP JSON               │
                               │  meta writes     →  server only             │
                               │  sessions        →  WS /trees/{id}/ws       │
                               │  auto-title      →  server-side trigger     │
                               │                                             │
                               │  ┌──────────────────────────────────────┐  │
                               │  │ Lifecycle Manager                    │  │
                               │  │                                      │  │
                               │  │  ACTIVE_WORKERS: Map<TreeId, Arc<E>> │  │
                               │  │  (global lock only for map lookup)   │  │
                               │  │                                      │  │
                               │  │  WorkerEntry (Arc, per-tree lock):   │  │
                               │  │  ├── stdin_tx   ──▶ commands         │  │
                               │  │  ├── ring buffer (Entry events)      │  │
                               │  │  ├── subscribers Vec<Sender>         │  │
                               │  │  └── pid, child handle               │  │
                               │  │                                      │  │
                               │  │  per worker: stdin writer thread,    │  │
                               │  │  stdout proxy thread, stderr demux   │  │
                               │  └──────────────────────────────────────┘  │
                               └─────────────────────────────────────────────┘

                               Worker subprocess (bwrap-sandboxed):
                               ┌──────────────────────────────────────────┐
                               │  stdin reader thread                     │
                               │    → parse WsCommand                     │
                               │    → set stop AtomicBool on Stop         │
                               │    → push AgentInput on mpsc             │
                               │                                          │
                               │  Agent thread (run_agent, unchanged)     │
                               │    ← AgentInput mpsc                     │
                               │    → ServerEvent mpsc                    │
                               │                                          │
                               │  stdout writer thread                    │
                               │    ← ServerEvent mpsc → JSON lines       │
                               └──────────────────────────────────────────┘
```

### Binary / crate structure

One binary (`agent`) with three subcommands, backed by library crates. Separate
crates enforce layering (worker cannot reach into server internals) and parallelize
compilation.

```
Cargo workspace:
  agent-core/     — lib: types, store, agent loop, tools, provider, rpc
  agent-server/   — lib: HTTP + WS server, lifecycle, auto-title trigger
  agent-worker/   — lib: stdin/stdout bridge, agent thread
  agent-cli/      — lib: TUI, WebSocket client
  agent/          — bin: dispatches to server / cli / worker
```

The worker subprocess is the same binary as the server, resolved via
`std::env::current_exe()` — no separate build step.

---

## Storage Restructure

The store currently writes to `~/.agent/trees/{id}.jsonl` +
`~/.agent/trees/{id}.meta.json`. To safely bind-mount only one tree's data into
its worker, restructure to per-tree directories:

```
~/.agent/
  config.toml
  trees/
    {tree-id-A}/
      data.jsonl
      meta.json
    {tree-id-B}/
      data.jsonl
      meta.json
```

The worker bwrap binds only `~/.agent/trees/{id}/` writable. Other trees'
directories and the parent `trees/` directory remain invisible — the worker
cannot read or write other trees' state.

**Server is the sole writer of `meta.json`.** Workers communicate desired meta
changes (e.g., auto-title result) via events; the server applies them. This
means a worker cannot redirect a future spawn by rewriting its own meta to
point at a different `repo_path` or escalated `sandbox` config.

Workers append-only to their own `data.jsonl`. The format already supports this
(JSONL append). On reads (context building, recovery), the server validates
that the file is well-formed.

---

## Per-Tree Sandbox Config

Stored in `meta.json` alongside other tree metadata. Empty config is the safe
default (`repo_path` writable, default credentials hidden, network on).

```rust
struct TreeSandbox {
    /// Writable bind-mounts in addition to the tree's repo_path.
    writable: Vec<PathBuf>,
    /// Override for network access. None = default (on).
    network:  Option<bool>,
    /// Additional credential directories to hide (tmpfs).
    hide:     Vec<PathBuf>,
    /// Credential directories from the global default list to NOT hide.
    unhide:   Vec<PathBuf>,
}
```

### Default credential blocklist

Hidden via tmpfs in every worker unless explicitly `unhide`'d. Curated list in
`[sandbox.defaults]` in `config.toml`:

```toml
[sandbox.defaults]
hide = [
    # Cloud / git / packaging
    "~/.ssh", "~/.aws", "~/.azure", "~/.config/gcloud", "~/.config/heroku",
    "~/.config/gh", "~/.config/glab", "~/.kube", "~/.docker",
    "~/.git-credentials", "~/.netrc",
    # Package registry tokens
    "~/.npmrc", "~/.pypirc", "~/.cargo/credentials.toml",
    # Crypto / keyrings
    "~/.gnupg", "~/.password-store",
    "~/.local/share/keyrings", "~/.config/keybase",
    # Shell histories (often contain pasted secrets)
    "~/.bash_history", "~/.zsh_history", "~/.local/share/fish/fish_history",
    # Browser cookie / session stores
    "~/.mozilla", "~/.config/google-chrome", "~/.config/chromium",
    # IM/desktop apps with tokens
    "~/.config/Slack", "~/.config/discord",
]
```

The list is configurable so it can be audited and extended. Non-existent paths
are skipped silently.

### Editing surface (no prompts)

- **At creation**: `agent create --writable ~/Code/foo --no-net "title"` —
  `repo_path` is auto-added to writable; other flags map to the struct.
- **Later**: `PATCH /trees/{id}` with a `sandbox` field. Takes effect on the
  next worker spawn for that tree. If a worker is currently active for the
  tree, the change is queued and applied on next spawn (no live reconfig).
- **No runtime prompts.** If the agent runs into EROFS or "permission denied"
  inside its sandbox, the tool returns the error to the LLM; the user fixes
  the sandbox config and re-spawns.

### `repo_path` validation

Performed at tree creation and rejected with a 400 if invalid:

- Reject `/`, `~`, `~`-relative roots, anything under `~/.agent` or
  `~/.config/agent`
- Reject any path in the resolved default `hide` list (after `unhide`)
- Canonicalize with `std::fs::canonicalize`; reject if it fails
- Require the path to exist and be a directory

---

## Protocol

### Tree CRUD (HTTP — same shape as today, plus sandbox)

```
GET    /trees
POST   /trees          { "title": "...", "repo_path": "...", "model": "...",
                         "sandbox": { ... } }
GET    /trees/{id}
PATCH  /trees/{id}     { "title": "...", "sandbox": { ... } }
GET    /trees/{id}/entries
POST   /trees/{id}/auto-title    # server-side, on demand
```

### Session channel (WebSocket)

`ws://host:8080/trees/{id}/ws`

Each WebSocket text frame is one JSON object.

**Client → Server (commands):**
```json
{"method":"message","params":{"text":"hello"}}
{"method":"stop"}
```

**Server → Client (events — reuses `ServerEvent`):**
```json
{"type":"text_chunk","content":"Hello!"}
{"type":"tool_start","tool":"bash","input":{"command":"ls"}}
{"type":"tool_result","tool":"bash","exit":0,"output":"src\n"}
{"type":"entry", ...}
{"type":"cap_warning","level":"soft","pct":68}
{"type":"meta_update", "title":"...", ...}
{"type":"done","status":"complete"}
{"type":"error","message":"...","fatal":false}
```

**HTTP and WS stack.** A single `std::net::TcpListener` accepts every
connection. Each accept spawns a thread that:

1. Reads bytes into a buffer, parses HTTP request line + headers with
   `httparse` until `\r\n\r\n`
2. If `Upgrade: websocket` is present and the path matches
   `/trees/{id}/ws`: extract `Sec-WebSocket-Key`, write the `101
   Switching Protocols` response with `Sec-WebSocket-Accept` computed
   via `tungstenite::handshake::derive_accept_key`, call
   `stream.set_nonblocking(true)`, then
   `tungstenite::WebSocket::from_raw_socket(stream, Role::Server, None)`
3. Otherwise: read Content-Length bytes for the body, dispatch via
   `match (method, path_segments)`, write a `Connection: close` JSON
   response, close

The HTTP layer is ~150 lines of code (parsing + dispatch + response
writing). No keep-alive — each request is one connection. Personal-use
traffic doesn't need it.

`tungstenite` is the WebSocket implementation on both server and CLI —
one crate, one API, one bug surface. It exposes `Message::Ping` /
`Message::Pong` / `Message::Close` directly, so we do real WS-level
keepalive: server sends a `Ping` every 30s; if no `Pong` within 90s,
server closes the connection and drops the subscriber.

Each WS connection's thread owns the `WebSocket` exclusively. Because
the underlying TcpStream is non-blocking, the loop reads with
`WouldBlock` as the "nothing to read right now" signal, then drains the
broadcast `Receiver<ServerEvent>` for outbound events, then sleeps
briefly. No `Mutex<WebSocket>`, no second thread.

### Ring buffer scope (changed from today)

The ring buffer keeps only `Entry` events. `TextChunk`, `ToolStart`,
`ToolResult`, and `CapWarning` are transient. A WS client that reconnects
mid-turn will not see prior chunks/tool deltas of the current turn — the
client is expected to re-fetch `GET /trees/{id}/entries` over HTTP if it
needs the full state, then attach for live events.

### Catch-up + subscribe atomicity

`WorkerEntry` is `Arc<Mutex<...>>` per tree. The proxy thread acquires the
entry's own lock when appending to the buffer and broadcasting. The WS handler
acquires the same lock to snapshot the buffer AND push its subscriber channel.
The global `ACTIVE_WORKERS` map lock is held only for map lookup; per-tree
work uses per-tree locks. This avoids serializing broadcasts across trees.

### Server ↔ Worker (stdin/stdout, newline-delimited JSON)

**Server → Worker stdin:** same `WsCommand` JSON, one per line. A single
**stdin-writer thread per worker** drains an mpsc `Sender<String>` into the
child's stdin pipe; multiple WS clients submitting commands cannot interleave.

**Worker stdout → Server:** same `ServerEvent` JSON, one per line. The proxy
thread parses each line, deserializes to `ServerEvent`, appends to the ring
buffer (Entry only), and fans out to subscribers under the entry's lock.

**Worker stderr → Server:** the server reads worker stderr line-by-line on a
dedicated thread and forwards each line to its own log (prefixed
`[worker {tree-id-short}] ...`). No mixing into the event stream.

---

## Worker (`agent-worker` lib)

### Threading model

```
stdin reader thread     →  AgentInput mpsc  ─┐
                          (also flips stop)  │
                                             ▼
                                       Agent thread (run_agent)
                                             │
stdout writer thread  ◀──── ServerEvent mpsc ┘
```

- **stdin thread**: reads one JSON line, parses `WsCommand`. On `Stop`, both
  pushes `AgentInput::Stop` to the mpsc *and* flips an `Arc<AtomicBool>` shared
  with the agent thread. The atomic is what the agent checks in its tight
  inner loops (`agent.rs:535`, `:555`) — without it, mid-LLM-stream stop is
  delayed until the stream ends.
- **Agent thread**: `run_agent()` from `agent-core` — unchanged.
- **stdout thread**: drains `ServerEvent` mpsc, writes JSON lines.

### Config passthrough

The worker needs `provider` (base_url, api_key, model), `summary`,
`session`, and logging settings. Approach:

- **Sensitive values via env vars** at spawn time (`LLM_API_KEY`, etc.). bwrap
  inherits the env unless `--clearenv` is set.
- **Non-sensitive settings via a bind-mounted config file**: `--ro-bind
  ~/.agent/config.toml ~/.agent/config.toml`. The worker loads the same way
  the server does, then env overrides apply.

The worker is invoked as:

```
agent worker --tree-id <id> --config ~/.agent/config.toml
```

### Hooks

Worker calls the same `hooks::startup_hooks` registration that the server
does, from the worker's own `main.rs`. This ensures tool-call and
before-LLM hooks behave identically to today. The hook registry lives in
`agent-core` already.

### Lifetime

Worker spawned by the server on first `message` (or first WS connection that
sends `message`). After that, the worker stays idle between turns,
blocking on its stdin mpsc. Worker exits only on:

- explicit `Stop` (writes `session_end` with `Aborted`, then exits)
- stdin closed (server is shutting down or restarted)
- panic / crash (server detects via stdout EOF + non-zero `waitpid`)

A `session_end` does not end the worker — multiple sessions over the worker's
lifetime are normal.

### Worker crash recovery

If the worker exits unexpectedly mid-turn (panic, OOM, SIGKILL):
1. Stdout EOF triggers the proxy thread to call `waitpid` and observe a
   non-zero exit
2. Server broadcasts `{"type":"error","message":"worker exited unexpectedly","fatal":true}`
3. Server appends a synthetic `session_end` with status `Aborted` to the
   tree's `data.jsonl`
4. Server broadcasts `{"type":"done","status":"aborted"}` and removes the
   entry from `ACTIVE_WORKERS`
5. Next `message` on that tree auto-spawns a fresh worker; it picks up from
   the new `session_end` boundary on next context build

On **server restart without clean shutdown**: scan each tree's `data.jsonl`
for trailing entries that aren't `session_end`. Append a synthetic
`session_end` (`Aborted`) before serving any traffic for that tree.

---

## bubblewrap Sandboxing

### bwrap command (per worker)

```bash
bwrap \
  --ro-bind / /                              \  # wide read; threat model permits
  --bind   <repo_path>           <repo_path> \  # writable: repo
  --bind   ~/.agent/trees/<id>/  ~/.agent/trees/<id>/ \  # writable: this tree only
  --ro-bind ~/.agent/config.toml ~/.agent/config.toml \
  --ro-bind <exe_path>           <exe_path>  \  # the agent binary
  --dev /dev                                 \
  --proc /proc                               \
  --tmpfs /tmp                               \
  <per-tree writable mounts from sandbox.writable> \
  <hide mounts from default + sandbox.hide, minus sandbox.unhide> \
  --unshare-all                              \
  <--share-net if sandbox.network unset or true> \
  --new-session                              \
  --die-with-parent                          \
  -- <exe_path> worker --tree-id <id> --config ~/.agent/config.toml
```

We keep `--ro-bind / /` deliberately. The threat model is "minimize the
impact of model stupidity," not "defend against an attacker enumerating
the filesystem." Wide read is cheap and unsurprising; the explicit
credential blocklist handles the named-secret cases. `/etc/resolv.conf`,
`/etc/ssl/certs`, etc. are picked up for free.

`--die-with-parent` ensures workers are killed if the server crashes
without a clean shutdown. The recovery path on next startup handles the
resulting unterminated sessions.

### Sandbox toggle

```toml
[sandbox]
enabled = true                              # false → direct spawn, no bwrap
bwrap_path = "/usr/bin/bwrap"               # auto-detected if omitted

[sandbox.defaults]
hide = [ ... see Default Credential Blocklist ... ]
```

When `enabled = false`, workers spawn directly via `std::process::Command`
with the same stdin/stdout pipe setup. Useful for development and CI.

---

## Server-Side Auto-Title

The worker doesn't generate titles. Instead, the server's stdout proxy thread
watches for `Done` events on a tree whose `meta.title` is `None`. When it
sees one, it spawns a side thread that:

1. Reads the tree's entries from the store
2. Builds a minimal context (first user message + first assistant response)
3. Calls `provider.chat()` non-streaming with the `summary` config
4. Updates `meta.json` (server is sole writer)
5. Broadcasts `{"type":"meta_update","title":"..."}` to WS subscribers

This lives entirely in `agent-server`, uses no sandbox, doesn't compete with
the worker's LLM stream, and works for any tree (including re-titling old
trees via `POST /trees/{id}/auto-title`).

---

## Graceful Shutdown

On SIGTERM/SIGINT:

1. Server stops accepting new HTTP/WS connections
2. For each active worker: write `{"method":"stop"}` to its stdin
3. Workers finish their current tool call, append `session_end` with status
   `Aborted`, then exit (stdout closes, proxy thread removes the entry)
4. Server waits up to 60s for all proxy threads to finish (in parallel — all
   workers receive `stop` before any waiting begins)
5. For workers that didn't exit: SIGKILL via `nix::sys::signal`. Server appends
   a synthetic `session_end` (Aborted) directly to the tree file
6. Server exits

---

## What Changes, What Stays

| Component       | Change                                  | Unchanged                    |
|-----------------|-----------------------------------------|------------------------------|
| `agent-core`    | Add `rpc.rs` (WsCommand + JSON helpers); store layout | Agent loop, tools, provider |
| `agent-core::store` | Per-tree subdirectory layout; only server writes meta | JSONL append semantics |
| `agent-server`  | WS endpoint, process lifecycle, auto-title trigger, sandbox config validation | HTTP tree CRUD shape |
| `agent-cli`     | WS transport for sessions; sandbox flags on `create` | TUI rendering              |
| `agent-worker`  | New crate                               | —                            |
| `agent` binary  | New crate                               | —                            |
| `run_agent()`   | —                                       | Unchanged                    |
| Tools           | —                                       | Unchanged                    |
| Hooks           | Initialized in worker `main.rs` too     | Registry, trait              |

---

## Implementation Steps

Each step produces working, testable code. **On completion of a step, check
its box and append a `**Notes:**` block directly below it** with: files
created/modified, decisions made, deviations from this plan, and the verify
command output. The notes stay in this file so the plan and the record stay
together. **Commit the changes after updating the step notes.**

Format per step:

```
### Step N — Title

- [x] done

**Goal:** ...

(bullets)

**Spec details:** (file paths, deps, type signatures, tests, do-not-modify)

**Verify:** ...

**Notes:**
- Created: agent-core/src/foo.rs
- Modified: ...
- Deviation: chose X over Y because ...
- Verified: cargo test --workspace → 80 passed
```

---

### Conventions (apply to every step)

These hold throughout. Don't re-explain them per step.

- **Derives on new types:** `Serialize, Deserialize, Clone, Debug` always;
  add `Default` where an empty value is meaningful; add `PartialEq` only
  if tests need it.
- **New fields on existing serialized types:** `#[serde(default)]` so older
  on-disk JSON keeps deserializing.
- **Error style:** `Result<T, String>` for internal call sites that match
  the existing pattern in `agent-server/src/lifecycle.rs`; `thiserror`
  enums for `agent-core` library errors (matches `store.rs`, `provider.rs`).
- **Logging:** `log::info!` / `warn!` / `error!`. Prefix multi-component
  logs with a bracketed tag like `[lifecycle]`, `[worker]`, `[ws]`.
- **File I/O:** call `std::fs::create_dir_all` before writes when the
  parent dir might not exist; use atomic `rename` for any non-append
  write that must not be observed half-written.
- **Cargo.lock:** never hand-edit. After Cargo.toml changes, run
  `cargo build` to regenerate.
- **No async runtime.** No `tokio`, no `async-std`. `std::thread` and
  `std::sync::mpsc` only.
- **`#[allow(dead_code)]` is forbidden** as a way to silence warnings.
  Either use the code or delete it.
- **Tests live with the code:** `#[cfg(test)] mod tests { ... }` at the
  bottom of the file containing the unit under test. Integration tests
  go in a `tests/` directory at the crate root.

---

### Step 1 — Store restructure

- [x] done

**Goal:** Per-tree subdirectory layout; server is sole writer of `meta.json`.

> **Pre-step:** This project has no users-in-the-wild. Before running the
> new code, delete any legacy data: `rm -rf ~/.agent/trees`. No migration
> code is needed and none will be written.

- Change `Store::jsonl_path` and `Store::meta_path` to use
  `trees/{id}/data.jsonl` and `trees/{id}/meta.json`
- Add `Store::tree_dir(id)` returning the per-tree directory (used by
  bwrap arg construction later)
- Add `TreeSandbox` type to `agent-core/src/types.rs`; extend `TreeMeta`
  with a `sandbox: TreeSandbox` field (default = empty)
- Document the "server is sole writer of meta" invariant in `store.rs`;
  no functional change here, but the worker code in later steps will
  respect it

**Spec details:**

Files modified:
- `agent-core/src/store.rs` — change `jsonl_path` and `meta_path`; add
  `tree_dir_for(id)`; `create_tree_file` and `save_tree_meta` must
  `create_dir_all(tree_dir_for(id))` before writing
- `agent-core/src/types.rs` — add `TreeSandbox`; add `sandbox` field to
  `TreeMeta`

New deps: none.

New types (in `agent-core/src/types.rs`):
```rust
use std::path::PathBuf;

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
```

Modify `TreeMeta`:
```rust
pub struct TreeMeta {
    // ... existing fields ...
    #[serde(default)]
    pub sandbox: TreeSandbox,
}
```

Path changes in `Store`:
```rust
fn meta_path(&self, id: &str) -> PathBuf {
    self.tree_dir().join(id).join("meta.json")
}
pub fn jsonl_path(&self, id: &str) -> PathBuf {
    self.tree_dir().join(id).join("data.jsonl")
}
pub fn tree_dir_for(&self, id: &str) -> PathBuf {
    self.tree_dir().join(id)
}
```

Tests to add:
- `test_create_tree_writes_subdir` (in `store.rs`) — `Store::new(tempdir)`,
  `create_tree_file("abc", "model")` + `save_tree_meta(...)`, assert
  `tempdir/trees/abc/data.jsonl` and `tempdir/trees/abc/meta.json` exist
- `test_tree_meta_sandbox_default` (in `types.rs`) — deserialize a
  `TreeMeta` JSON without the `sandbox` field; assert
  `meta.sandbox == TreeSandbox::default()`

Do not modify: `agent.rs`, tools, provider, hooks, CLI, server routes,
lifecycle. Only `store.rs` and `types.rs`.

**Verify:** `cargo test --workspace` green. Manually: `rm -rf
~/.agent/trees`, start the server, create a tree via the CLI, observe
`~/.agent/trees/{id}/data.jsonl` + `meta.json` on disk.

**Notes:**
- Created: none
- Modified: `agent-core/src/types.rs` — added `TreeSandbox` struct, added `sandbox` field to `TreeMeta`
- Modified: `agent-core/src/store.rs` — changed `meta_path`/`jsonl_path` to use `{id}/meta.json` and `{id}/data.jsonl`; added public `tree_dir_for()`; changed `rebuild_index` to scan subdirectories; updated all test `TreeMeta` constructors
- Modified: `agent-core/src/tools/search.rs` — updated `search_trees` to walk subdirectories for `data.jsonl` instead of flat `*.jsonl` files; removed unused `BufRead` import
- Modified: `agent-server/src/routes.rs` — added `sandbox: TreeSandbox::default()` to `handle_create_tree`
- Test added: `test_tree_meta_sandbox_default` in `types.rs`
- Test added: `test_create_tree_writes_subdir` in `store.rs`
- Deviation: `rebuild_index` also needed updating from flat file scan to subdirectory scan; `search_trees` in search.rs also needed updating for the same reason
- Verified: `cargo test --workspace` → 77 passed, 0 failed

---

### Step 2 — Protocol types + agent-worker skeleton

- [x] done

**Goal:** `agent worker` subcommand compiles and bridges stdin/stdout to the
agent loop. No sandbox yet.

- Add to `agent-core`: `rpc.rs` with `WsCommand` enum and JSON line helpers
- Create `agent-worker/` crate with stdin reader, agent thread, stdout writer
- Create `agent/` binary crate that dispatches subcommands

**Spec details:**

Files created:
- `agent-core/src/rpc.rs`
- `agent-worker/Cargo.toml`
- `agent-worker/src/lib.rs`
- `agent/Cargo.toml`
- `agent/src/main.rs`

Files modified:
- `agent-core/src/lib.rs` — add `pub mod rpc;`
- `agent-core/src/config.rs` — add `pub fn load_config_from_path(path: &Path) -> Config`
  (refactor the existing `load_config` body to take a path; have `load_config`
  call it with the default path)
- Root `Cargo.toml` — add `"agent-worker"` and `"agent"` to `workspace.members`

New deps:
- `agent-worker/Cargo.toml`:
  ```toml
  [dependencies]
  agent-core = { path = "../agent-core" }
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  log = "0.4"
  env_logger = "0.11"
  ```
- `agent/Cargo.toml`:
  ```toml
  [dependencies]
  agent-server = { path = "../agent-server" }
  agent-cli = { path = "../agent-cli" }
  agent-worker = { path = "../agent-worker" }
  ```

`WsCommand` shape (`agent-core/src/rpc.rs`) — this serde shape produces
exactly the JSON the WS protocol section describes:
```rust
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum WsCommand {
    Message { params: MessageParams },
    Stop,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageParams {
    pub text: String,
}
```
This serializes `WsCommand::Message{params: MessageParams{text: "x"}}` as
`{"method":"message","params":{"text":"x"}}` and `WsCommand::Stop` as
`{"method":"stop"}`.

JSON line helpers (same file):
```rust
pub fn write_json_line<W: Write, T: Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let s = serde_json::to_string(value).map_err(std::io::Error::other)?;
    w.write_all(s.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

pub fn read_json_line<R: BufRead, T: serde::de::DeserializeOwned>(
    r: &mut R,
    buf: &mut String,
) -> std::io::Result<Option<T>> {
    buf.clear();
    match r.read_line(buf)? {
        0 => Ok(None),
        _ => Ok(Some(serde_json::from_str(buf.trim_end()).map_err(std::io::Error::other)?)),
    }
}
```

Worker entry point (`agent-worker/src/lib.rs`):
```rust
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (tree_id, config_path) = parse_argv()?;
    let config = agent_core::config::load_config_from_path(&config_path);
    agent_core::logging::init_logging(/* from config */);
    agent_core::hooks::run_startup_hooks().ok();

    let store = agent_core::store::Store::default();
    let provider = /* build from config.provider */;

    let (input_tx, input_rx) = std::sync::mpsc::channel::<agent_core::types::AgentInput>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<agent_core::types::ServerEvent>();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // stdin reader thread: parse WsCommand, push AgentInput, flip stop on Stop
    let stop_for_stdin = stop.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buf = String::new();
        loop {
            match agent_core::rpc::read_json_line::<_, agent_core::rpc::WsCommand>(&mut reader, &mut buf) {
                Ok(Some(WsCommand::Message { params })) => {
                    let _ = input_tx.send(AgentInput::Message { text: params.text });
                }
                Ok(Some(WsCommand::Stop)) => {
                    stop_for_stdin.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = input_tx.send(AgentInput::Stop);
                }
                Ok(None) | Err(_) => break, // EOF or parse error
            }
        }
    });

    // stdout writer thread
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        for event in event_rx {
            let _ = agent_core::rpc::write_json_line(&mut writer, &event);
        }
    });

    // Run agent in main thread
    agent_core::agent::run_agent(
        &tree_id, store, provider, config.session,
        input_rx, event_tx, stop,
    );
    Ok(())
}
```

`agent/src/main.rs`:
```rust
fn main() {
    let mut args = std::env::args();
    let _ = args.next(); // exe name
    let sub = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    match sub.as_str() {
        "server" => agent_server::run(rest),
        "cli" => agent_cli::run(rest),
        "worker" => {
            if let Err(e) = agent_worker::run() {
                eprintln!("worker: {}", e);
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown subcommand: {}\nusage: agent <server|cli|worker> ...", other);
            std::process::exit(2);
        }
    }
}
```
This requires `agent_server` and `agent_cli` to expose `pub fn run(args:
Vec<String>)`. Add those wrappers (the current `main` body becomes the
body of `run`).

Tests to add:
- `test_wscommand_message_roundtrip` (in `rpc.rs`) — serialize a
  `WsCommand::Message{params: MessageParams{text: "hi"}}`, assert the
  string equals `{"method":"message","params":{"text":"hi"}}`; deserialize
  back, assert variant + text
- `test_wscommand_stop_no_params` (in `rpc.rs`) — deserialize the literal
  string `{"method":"stop"}`, assert `WsCommand::Stop`

Do not modify: `agent-core/src/agent.rs`, tools, hooks (other than calling
`run_startup_hooks` from worker), provider, store layout, server routes.

**Verify:** With an existing tree id created via the current server,
```
echo '{"method":"message","params":{"text":"list files"}}' | \
  cargo run -p agent --bin agent -- worker --tree-id <id> --config ~/.agent/config.toml
```
emits JSON event lines on stdout (TextChunk + Done at minimum).

**Notes:**
- Created: `agent-core/src/rpc.rs` — `WsCommand` enum, `MessageParams`, JSON line helpers (`write_json_line`, `read_json_line`), plus roundtrip tests
- Created: `agent-worker/Cargo.toml`, `agent-worker/src/lib.rs` — worker entry point with stdin reader thread, agent thread, stdout writer thread; simple `parse_argv` for `--tree-id` and `--config`
- Created: `agent/Cargo.toml`, `agent/src/main.rs` — unified binary with `server`, `cli`, `worker` subcommands
- Created: `agent-server/src/lib.rs` — `pub fn run(args)` (refactored from main)
- Created: `agent-cli/src/lib.rs` — `pub fn run(args)` (refactored from main, all helper functions moved from main.rs)
- Modified: `agent-core/src/lib.rs` — added `pub mod rpc`
- Modified: `agent-core/src/config.rs` — refactored `load_config` into `load_config_from_path(path)` + `load_config()` that calls it with default path
- Modified: `agent-server/src/main.rs` — thin wrapper calling `agent_server::run()`
- Modified: `agent-cli/src/main.rs` — thin wrapper calling `agent_cli::run()`
- Modified: `root Cargo.toml` — added `agent-worker` and `agent` to workspace members
- Deviation: `agent-cli/src/lib.rs` needs `Cli::parse_from` with prepended program name since `run()` receives args without the executable name
- Verified: `cargo test --workspace` → 79 passed, 0 failed; `cargo clippy --workspace` → no new warnings

---

### Step 3a — Hand-rolled HTTP layer on TcpListener + httparse

- [ ]

**Goal:** Replace `rouille` with a small in-tree HTTP layer built on
`std::net::TcpListener` + `httparse`. All seven routes (Connection: close,
JSON request/response only). SSE on the old session route still works,
ported to the new layer. WS comes in 3c.

**Spec details:**

Files modified:
- `agent-server/Cargo.toml` — remove `rouille`, add `httparse = "1"`
- `agent-server/src/main.rs` — replace `rouille::start_server` with a
  `TcpListener::bind` + accept loop
- `agent-server/src/routes.rs` — replace `router!` with a `dispatch()`
  function taking parsed request parts
- `agent-server/src/http.rs` — **new file**: per-connection handler

New deps:
```toml
httparse = "1"
```
(remove `rouille`).

`agent-server/src/main.rs` accept loop:
```rust
use std::net::TcpListener;
let bind = format!("{}:{}", config.server.host, config.server.port);
let listener = TcpListener::bind(&bind).expect("bind");
log::info!("Listening on http://{}", bind);
for stream in listener.incoming() {
    let Ok(stream) = stream else { continue };
    let store = store.clone();
    let cfg = cfg.clone();
    std::thread::spawn(move || crate::http::handle_connection(stream, store, cfg));
}
```

`agent-server/src/http.rs` (per-connection handler):
```rust
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use agent_core::config::Config;
use agent_core::store::Store;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES:   usize = 4 * 1024 * 1024;

pub fn handle_connection(mut stream: TcpStream, store: Arc<Store>, cfg: Arc<Config>) {
    // Slowloris guard: a misbehaving client could open a TCP connection and
    // dribble bytes forever, pinning a thread. 30s read timeout closes the
    // connection if no progress is made between reads. Required, not optional.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let header_end;
    loop {
        if buf.len() > MAX_HEADER_BYTES {
            write_status(&mut stream, 431, "Request Header Fields Too Large");
            return;
        }
        let mut tmp = [0u8; 1024];
        let n = match stream.read(&mut tmp) { Ok(0) => return, Ok(n) => n, Err(_) => return };
        buf.extend_from_slice(&tmp[..n]);
        let mut hs = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut hs);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => { header_end = n; break; }
            Ok(httparse::Status::Partial)     => continue,
            Err(_) => { write_status(&mut stream, 400, "Bad Request"); return; }
        }
    }
    // Re-parse to get owned views of method/path/headers.
    let mut hs = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut hs);
    let _ = req.parse(&buf);

    let method = req.method.unwrap_or("").to_string();
    let path   = req.path.unwrap_or("").to_string();
    let headers: Vec<(String, Vec<u8>)> = hs.iter()
        .filter(|h| !h.name.is_empty())
        .map(|h| (h.name.to_string(), h.value.to_vec()))
        .collect();

    // WS upgrade?
    let is_ws = method == "GET"
        && header_contains(&headers, "upgrade", b"websocket")
        && header_contains(&headers, "connection", b"upgrade");
    if is_ws {
        crate::ws::accept(stream, &path, &headers, store, cfg);
        return;
    }

    // Read body up to Content-Length
    let content_length: usize = header_get(&headers, "content-length")
        .and_then(|v| std::str::from_utf8(&v).ok()?.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        write_status(&mut stream, 413, "Payload Too Large"); return;
    }
    let need = header_end + content_length;
    while buf.len() < need {
        let mut tmp = [0u8; 4096];
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return,
        }
    }
    let body = &buf[header_end..need.min(buf.len())];

    // Dispatch to routes::dispatch — returns (status, body_bytes).
    let (status, body_bytes, content_type) =
        crate::routes::dispatch(&method, &path, body, &store, &cfg);
    write_response(&mut stream, status, &body_bytes, content_type);
}

fn header_get<'a>(headers: &'a [(String, Vec<u8>)], name: &str) -> Option<Vec<u8>> {
    headers.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}
fn header_contains(headers: &[(String, Vec<u8>)], name: &str, needle: &[u8]) -> bool {
    header_get(headers, name).map(|v| {
        v.split(|&b| b == b',')
            .any(|t| t.trim_ascii().eq_ignore_ascii_case(needle))
    }).unwrap_or(false)
}

fn write_status(w: &mut TcpStream, code: u16, reason: &str) {
    let _ = write!(w, "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", code, reason);
}
fn write_response(w: &mut TcpStream, status: u16, body: &[u8], content_type: &str) {
    let _ = write!(w,
        "HTTP/1.1 {} OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        status, content_type, body.len()
    );
    let _ = w.write_all(body);
}
```
(`<[u8]>::trim_ascii` is stable; if your toolchain is older, replace with a small helper.)

`agent-server/src/routes.rs` becomes:
```rust
use std::sync::Arc;
use agent_core::config::Config;
use agent_core::store::Store;

pub fn dispatch(method: &str, path: &str, body: &[u8], store: &Arc<Store>, cfg: &Arc<Config>)
    -> (u16, Vec<u8>, &'static str)
{
    // Static
    if method == "GET" && path == "/" {
        return json(200, &serde_json::json!({"service":"agent-server","version":"0.1.0"}));
    }
    if method == "GET" && path == "/trees" { return handle_list_trees(store); }
    if method == "POST" && path == "/trees" { return handle_create_tree(body, store); }
    // Parameterized
    if let Some(rest) = path.strip_prefix("/trees/") {
        let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        return match (method, suffix) {
            ("GET",   "")           => handle_get_tree(id, store),
            ("PATCH", "")           => handle_update_tree(id, body, store),
            ("GET",   "entries")    => handle_list_entries(id, store),
            ("POST",  "auto-title") => handle_auto_title(id, store, cfg),
            _ => not_found(),
        };
    }
    not_found()
}

fn json<T: serde::Serialize>(status: u16, v: &T) -> (u16, Vec<u8>, &'static str) {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    (status, body, "application/json")
}
fn not_found() -> (u16, Vec<u8>, &'static str) {
    json(404, &serde_json::json!({"error":"not found"}))
}
```
Each `handle_*` helper now takes `body: &[u8]` instead of `&Request`, and
parses via `serde_json::from_slice`. Refactor the existing handlers
accordingly.

SSE in this step: port the existing `SseUpgrade` body into a function
that writes the SSE 200 response directly to the `TcpStream`, then
loops over the broadcast receiver writing `data: {json}\n\n`. Single
thread per SSE connection, owns the stream. SSE goes away in Step 7
along with `POST /trees/{id}/message`.

Tests to add:
- `test_dispatch_static` (in `routes.rs`) — call `dispatch("GET", "/", ...)`,
  assert `(200, body, "application/json")`
- `test_dispatch_tree_param` — `("GET", "/trees/abc/entries", ...)` calls
  the right handler (refactor to check via a flag or via behavior on a
  test Store)
- `test_header_contains_case_insensitive` (in `http.rs`)
- `test_http_buffer_limit` — feed a header buffer past MAX_HEADER_BYTES,
  assert 431 response

Do not modify: `agent-core`, `agent-cli`, `agent-worker`, `lifecycle.rs`
(other than imports that referenced rouille types).

**Verify:** `cargo test --workspace` green. Manual: existing CLI
(`agent-cli msg <id> "..."`) still works end-to-end via the SSE path
through the new layer.

**Notes:**
_(fill in on completion)_

---

### Step 3b — Worker subprocess lifecycle

- [ ]

**Goal:** Server can spawn worker subprocesses and proxy their stdin/stdout
to/from in-process channels. Still no WS endpoint — sessions can be exercised
via a temporary HTTP test handler.

**Spec details:**

Files modified:
- `agent-server/src/lifecycle.rs` — add new `WorkerEntry` + `ACTIVE_WORKERS`;
  the old `AgentHandle` + `ACTIVE_AGENTS` stays in place for now and is
  removed in Step 7

New types (in `lifecycle.rs`):
```rust
const BUFFER_CAPACITY: usize = 1000;

pub struct WorkerEntry {
    pub stdin_tx: mpsc::Sender<String>,
    pub event_buffer: VecDeque<ServerEvent>,           // Entry events only
    pub subscribers: Vec<mpsc::Sender<ServerEvent>>,
    pub pid: u32,
    pub child: Option<std::process::Child>,
}

pub static ACTIVE_WORKERS: LazyLock<Mutex<HashMap<TreeId, Arc<Mutex<WorkerEntry>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn worker_get(tree_id: &str) -> Option<Arc<Mutex<WorkerEntry>>> {
    ACTIVE_WORKERS.lock().unwrap().get(tree_id).cloned()
}
```

`spawn_worker` shape:
```rust
pub fn spawn_worker(tree_id: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let config_path = agent_core::config::agent_dir().join("config.toml");

    let mut child = Command::new(&exe)
        .arg("worker")
        .arg("--tree-id").arg(tree_id)
        .arg("--config").arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn worker: {e}"))?;

    let pid = child.id();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>();

    let entry = Arc::new(Mutex::new(WorkerEntry {
        stdin_tx,
        event_buffer: VecDeque::with_capacity(BUFFER_CAPACITY),
        subscribers: Vec::new(),
        pid,
        child: Some(child),
    }));

    ACTIVE_WORKERS.lock().unwrap().insert(tree_id.to_string(), entry.clone());

    spawn_stdin_writer(stdin, stdin_rx);
    spawn_stdout_proxy(tree_id.to_string(), stdout, entry.clone());
    spawn_stderr_demux(tree_id.to_string(), stderr);
    Ok(())
}
```

Helper threads:
```rust
fn spawn_stdin_writer(mut stdin: std::process::ChildStdin, rx: mpsc::Receiver<String>) {
    std::thread::spawn(move || {
        use std::io::Write;
        while let Ok(line) = rx.recv() {
            if writeln!(stdin, "{}", line).is_err() { break; }
            if stdin.flush().is_err() { break; }
        }
    });
}

fn spawn_stdout_proxy(
    tree_id: String,
    stdout: std::process::ChildStdout,
    entry: Arc<Mutex<WorkerEntry>>,
) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let event: ServerEvent = match serde_json::from_str(buf.trim_end()) {
                Ok(e) => e,
                Err(e) => { log::warn!("[proxy {}] bad event JSON: {}", tree_id, e); continue; }
            };
            let mut guard = entry.lock().unwrap();
            if matches!(event, ServerEvent::Entry(_)) {
                if guard.event_buffer.len() >= BUFFER_CAPACITY {
                    guard.event_buffer.pop_front();
                }
                guard.event_buffer.push_back(event.clone());
            }
            guard.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
        }
        log::info!("[proxy {}] worker stdout closed", tree_id);
        ACTIVE_WORKERS.lock().unwrap().remove(&tree_id);
    });
}

fn spawn_stderr_demux(tree_id: String, stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut buf = String::new();
        let short = &tree_id[..tree_id.len().min(8)];
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => log::info!("[worker {}] {}", short, buf.trim_end()),
            }
        }
    });
}
```

Public API (in `lifecycle.rs`):
- `pub fn worker_send_command(tree_id: &str, json_line: &str) -> Result<(), String>`
- `pub fn worker_stop(tree_id: &str) -> Result<(), String>` — sends
  `{"method":"stop"}` on stdin
- `pub fn worker_subscribe(tree_id: &str) -> Option<(Vec<ServerEvent>, mpsc::Receiver<ServerEvent>)>` —
  under the entry's lock, snapshot the ring buffer AND push a new
  subscriber Sender; return both atomically

Tests to add:
- `test_worker_subscribe_atomicity` (in `lifecycle.rs`) — manual test that
  inserts a fake `WorkerEntry`, calls `worker_subscribe` while another
  thread is appending events; assert no events are dropped between the
  snapshot and the live subscription

Do not modify: routes (other than wiring; the WS endpoint comes in 3c),
agent-core, agent-cli, agent-worker.

**Verify:** Add a temporary test route `POST /trees/{id}/_test_spawn` that
calls `spawn_worker(id)` and a `POST /trees/{id}/_test_send` that calls
`worker_send_command`. Run the server; spawn a worker; send a message;
inspect server logs for proxied events. (Remove these test routes in 3c
once the real WS endpoint exists.)

**Notes:**
_(fill in on completion)_

---

### Step 3c — WS endpoint + keepalive

- [ ]

**Goal:** `GET /trees/{id}/ws` upgrades to a WebSocket on the same
TcpListener as HTTP. Events flow from the worker via the proxy thread
to all WS subscribers; commands flow the other way.

**Spec details:**

Files modified:
- `agent-server/Cargo.toml` — add `tungstenite = "0.21"`,
  `sha1_smol = "1"` (used by `derive_accept_key`; transitive of
  tungstenite but make it explicit if needed)
- `agent-server/src/routes.rs` — drop the temporary test routes from 3b

New deps:
```toml
tungstenite = { version = "0.21", default-features = false }
```

New file: `agent-server/src/ws.rs`
```rust
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::config::Config;
use agent_core::store::Store;
use tungstenite::Message;

/// Called by http::handle_connection when it detects a WS upgrade request.
/// `headers` are the parsed (name, value) pairs from the request.
pub fn accept(
    mut stream: TcpStream,
    path: &str,
    headers: &[(String, Vec<u8>)],
    store: Arc<Store>,
    cfg: Arc<Config>,
) {
    // Extract tree id from path: /trees/{id}/ws
    let tree_id = match path.strip_prefix("/trees/").and_then(|r| r.strip_suffix("/ws")) {
        Some(id) if !id.is_empty() && !id.contains('/') => id.to_string(),
        _ => { let _ = write_400(&mut stream, "bad ws path"); return; }
    };

    // Extract Sec-WebSocket-Key
    let key = match get_header(headers, "sec-websocket-key") {
        Some(k) => k,
        None => { let _ = write_400(&mut stream, "missing Sec-WebSocket-Key"); return; }
    };

    // Compute Sec-WebSocket-Accept and send 101.
    let accept = tungstenite::handshake::derive_accept_key(key.trim().as_bytes());
    if write!(stream,
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n", accept).is_err() { return; }

    // Now we own the raw TcpStream. Set non-blocking BEFORE wrapping.
    if stream.set_nonblocking(true).is_err() { return; }

    let mut ws = tungstenite::WebSocket::from_raw_socket(
        stream, tungstenite::protocol::Role::Server, None
    );

    // Ensure the worker exists for this tree.
    if crate::lifecycle::worker_get(&tree_id).is_none() {
        if let Err(e) = crate::lifecycle::spawn_worker(&tree_id) {
            let _ = ws.send(Message::Text(serde_json::to_string(&serde_json::json!({
                "type": "error", "message": e, "fatal": true,
            })).unwrap()));
            return;
        }
    }

    // Snapshot ring buffer + push subscriber atomically.
    let Some((catch_up, rx)) = crate::lifecycle::worker_subscribe(&tree_id) else { return };
    for ev in catch_up {
        if let Ok(s) = serde_json::to_string(&ev) {
            if ws.send(Message::Text(s)).is_err() { return; }
        }
    }

    run_session(tree_id, ws, rx);
}

fn run_session(
    tree_id: String,
    mut ws: tungstenite::WebSocket<TcpStream>,
    rx: std::sync::mpsc::Receiver<agent_core::types::ServerEvent>,
) {
    let mut last_ping = Instant::now();
    let mut last_pong = Instant::now();

    loop {
        // 1. Try to read one inbound message (non-blocking)
        match ws.read() {
            Ok(Message::Text(s)) => {
                let _ = crate::lifecycle::worker_send_command(&tree_id, &s);
            }
            Ok(Message::Pong(_)) => { last_pong = Instant::now(); }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        // 2. Drain any outbound events
        while let Ok(ev) = rx.try_recv() {
            if let Ok(s) = serde_json::to_string(&ev) {
                if ws.send(Message::Text(s)).is_err() { return; }
            }
        }
        // 3. Keepalive
        let now = Instant::now();
        if now.duration_since(last_ping) > Duration::from_secs(30) {
            let _ = ws.send(Message::Ping(Vec::new()));
            last_ping = now;
        }
        if now.duration_since(last_pong) > Duration::from_secs(90) {
            log::warn!("[ws {}] pong timeout", tree_id);
            let _ = ws.send(Message::Close(None));
            break;
        }
        // INTENTIONAL: 10ms sets the latency floor for both inbound commands
        // and outbound event delivery. For personal-use traffic (a handful of
        // concurrent WS, LLM chunks arriving every 10–30ms anyway) this is
        // invisible and costs ~negligible CPU. DO NOT replace with
        // `Thread::yield_now()` (burns a core) or remove (burns harder) or
        // shorten without measuring. The architecturally clean alternative is
        // edge-triggered I/O via mio + an eventfd written by the proxy thread
        // on broadcast — only worth it past ~100 concurrent sessions.
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn get_header(headers: &[(String, Vec<u8>)], name: &str) -> Option<String> {
    headers.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .map(|s| s.to_string())
}

fn write_400(stream: &mut TcpStream, msg: &str) -> std::io::Result<()> {
    let body = format!("{{\"error\":\"{}\"}}", msg);
    write!(stream,
        "HTTP/1.1 400 Bad Request\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(), body)
}
```

Note: `stream.set_nonblocking(true)` supersedes any
`set_read_timeout` inherited from the HTTP layer — those are
mutually exclusive modes on POSIX, and non-blocking wins. Safe to
ignore the leftover timeout; no need to clear it explicitly.

`agent-server/src/lib.rs` needs `pub mod ws;` so `http.rs` can call
`crate::ws::accept`.

Tests to add:
- `test_ws_handshake_then_message` (integration test in
  `agent-server/tests/ws.rs`) — `TcpListener::bind` an ephemeral port,
  spawn an accept thread that calls `http::handle_connection`, connect
  with `tungstenite::client::connect`, send
  `{"method":"stop"}`, assert the connection stays open and the
  underlying worker receives the line (use a stub worker or just
  assert the test doesn't panic)
- `test_derive_accept_key_matches_rfc` (in `ws.rs`) — RFC 6455 example:
  key `"dGhlIHNhbXBsZSBub25jZQ=="` → accept
  `"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="`

Do not modify: agent-core, agent-cli, agent-worker.

**Verify:** `wscat -c ws://localhost:8080/trees/<id>/ws`, send
`{"method":"message","params":{"text":"hi"}}`, see streamed events.

**Notes:**
_(fill in on completion)_

---

### Step 4 — CLI: WebSocket transport

- [ ]

**Goal:** CLI session commands go over WebSocket. Tree CRUD stays HTTP.

**Spec details:**

Files modified:
- `agent-cli/Cargo.toml` — add `tungstenite = "0.21"`, `url = "2"`
- `agent-cli/src/client.rs` — add `AgentSession`
- `agent-cli/src/interactive.rs` — use `AgentSession` instead of HTTP+SSE
- `agent-cli/src/main.rs` — `msg` subcommand uses `AgentSession`
- Drop the SSE parser in `client.rs` (it's no longer used; remove only
  if no other code path needs it — otherwise leave for Step 7 cleanup)

New deps:
```toml
tungstenite = { version = "0.21", default-features = false }
url = "2"
```

`AgentSession` API:
```rust
pub struct AgentSession {
    ws: tungstenite::WebSocket<std::net::TcpStream>,
}

impl AgentSession {
    pub fn connect(host: &str, port: u16, tree_id: &str) -> Result<Self, String> {
        // Same port as HTTP — server multiplexes HTTP and WS on one listener.
        let url = format!("ws://{}:{}/trees/{}/ws", host, port, tree_id);
        let (ws, _resp) = tungstenite::connect(url).map_err(|e| e.to_string())?;
        Ok(Self { ws })
    }
    pub fn send_message(&mut self, text: &str) -> Result<(), String> {
        let cmd = agent_core::rpc::WsCommand::Message {
            params: agent_core::rpc::MessageParams { text: text.into() },
        };
        let s = serde_json::to_string(&cmd).map_err(|e| e.to_string())?;
        self.ws.send(tungstenite::Message::Text(s)).map_err(|e| e.to_string())
    }
    pub fn send_stop(&mut self) -> Result<(), String> { /* ... */ }
    pub fn next_event(&mut self) -> Option<Result<agent_core::types::ServerEvent, String>> {
        loop {
            match self.ws.read() {
                Ok(tungstenite::Message::Text(s)) => {
                    return Some(serde_json::from_str(&s).map_err(|e| e.to_string()));
                }
                Ok(tungstenite::Message::Ping(p)) => {
                    let _ = self.ws.send(tungstenite::Message::Pong(p));
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => return None,
                _ => {}
            }
        }
    }
}
```

No new CLI flags: HTTP and WS share the existing `--server host:port`
flag. `AgentSession::connect` parses host and port out of it.

Tests to add:
- `test_agent_session_url_format` — assert the constructed URL string

Do not modify: agent-server, agent-core, agent-worker.

**Verify:** `agent cli msg <id> "hello"` and the interactive TUI both
work end-to-end via WS.

**Notes:**
_(fill in on completion)_

---

### Step 5 — Graceful shutdown + crash recovery

- [ ]

**Goal:** Clean shutdown on SIGINT/SIGTERM; tree recovery on worker crash
or unclean restart.

**Spec details:**

Files modified:
- `agent-server/Cargo.toml` — add `signal-hook = "0.3"`
- `agent-server/src/main.rs` — install signal handler, shutdown loop
- `agent-server/src/lifecycle.rs` — `shutdown_all()`, helpers for
  synthetic `session_end` and crash detection
- `agent-core/src/store.rs` — `read_last_entry(tree_id)`,
  `scan_for_unterminated()` (or equivalent) for the startup recovery scan

New deps:
```toml
signal-hook = "0.3"
```

Signal install (in `agent-server/src/main.rs`):
```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
let shutting_down = Arc::new(AtomicBool::new(false));
let s = shutting_down.clone();
signal_hook::flag::register(signal_hook::consts::SIGINT, s.clone()).unwrap();
signal_hook::flag::register(signal_hook::consts::SIGTERM, s).unwrap();
// Main loop checks shutting_down; on true, call lifecycle::shutdown_all()
// and exit incoming-request acceptance.
```

`lifecycle::shutdown_all()` semantics:
1. Snapshot the active workers list under the global map lock; release
2. For each worker: `worker_stop(id)` (writes `{"method":"stop"}` to
   stdin)
3. Spawn a `Vec<JoinHandle>` of threads that each call
   `child.wait_timeout(Duration::from_secs(60))` on the worker's `Child`
   (use `wait-timeout` crate or implement via `nix::sys::wait` polling).
   Simpler: poll `child.try_wait()` in a loop with 100ms sleeps until 60s
4. For any worker still alive: `nix::sys::signal::kill(pid, SIGKILL)`;
   append a synthetic `session_end` (Aborted) directly to the tree's
   `data.jsonl` via the store

Crash detection (already partially present in `spawn_stdout_proxy`):
when stdout EOF arrives, also call `child.try_wait()`. If exit status
is non-zero OR the proxy didn't see a `Done` event recently, broadcast
`error{fatal: true}` and `done{status: "aborted"}`, then append a
synthetic `session_end`.

Startup scan (`store.rs`):
```rust
pub fn scan_unterminated(&self) -> Vec<TreeId> {
    // For each tree, read the last entry from data.jsonl using BufReader
    // back-scan or a simple full read. Return ids whose last entry is
    // not Entry::SessionEnd.
}
```

In `main.rs`, after `rebuild_index`: call `store.scan_unterminated()`;
for each, append a synthetic `session_end` (`Aborted`).

Synthetic `session_end` helper (in `agent-server/src/lifecycle.rs`):
```rust
fn append_synthetic_session_end(store: &Store, tree_id: &str) {
    use agent_core::types::{Entry, SessionStatus};
    let entry = Entry::SessionEnd {
        id: generate_entry_id(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: Some("session aborted (worker exit or server shutdown)".into()),
        status: SessionStatus::Aborted,
        continuation_brief: None,
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        log::error!("[lifecycle] append synthetic session_end for {}: {}", tree_id, e);
    }
}
```
(`generate_entry_id` already exists in `routes.rs` — promote it to
`agent-core` if it isn't there already, in a small `util.rs`.)

Tests to add:
- `test_scan_unterminated` — write a fake tree with a `session_start` +
  `message` but no `session_end`; assert it's returned
- `test_synthetic_session_end_appended` — append, then read all entries,
  assert the last is `SessionEnd { status: Aborted, .. }`

Do not modify: agent-core/agent.rs, tools, CLI.

**Verify:**
1. `agent server` with active worker → SIGINT → `tail -n3 ~/.agent/trees/<id>/data.jsonl`
   shows `session_end`
2. `kill -9 <worker-pid>` → server log shows error+done events; next
   `agent cli msg <id> ...` spawns a fresh worker that picks up from the
   recovery point

**Notes:**
_(fill in on completion)_

---

### Step 6 — Server-side auto-title + meta-update events

- [ ]

**Goal:** When the proxy thread sees `Done` on a tree whose `meta.title`
is `None`, fire `auto_title` server-side and broadcast a `meta_update`.

**Spec details:**

Files modified:
- `agent-core/src/types.rs` — add `ServerEvent::MetaUpdate` variant
- `agent-server/src/lifecycle.rs` — hook auto-title in `spawn_stdout_proxy`
- `agent-server/src/routes.rs` — `handle_auto_title` already exists; ensure
  it broadcasts `MetaUpdate` to current subscribers after success

New variant:
```rust
#[serde(rename = "meta_update")]
MetaUpdate { title: Option<String> },
```

Hook point in `spawn_stdout_proxy` (after broadcasting an event):
```rust
if matches!(event, ServerEvent::Done { .. }) {
    let store_for_title = store.clone();
    let cfg_for_title = cfg.clone();
    let entry_for_title = entry.clone();
    let tid = tree_id.clone();
    std::thread::spawn(move || {
        let needs = match store_for_title.get_tree(&tid) {
            Ok(Some(m)) => m.title.is_none(),
            _ => false,
        };
        if !needs { return; }
        let provider = Provider::new(
            cfg_for_title.summary.base_url.clone(),
            cfg_for_title.summary.api_key.clone(),
            cfg_for_title.summary.model.clone(),
        );
        match agent_core::agent::auto_title(&store_for_title, &provider, &tid) {
            Ok(title) => {
                let ev = ServerEvent::MetaUpdate { title: Some(title) };
                let mut g = entry_for_title.lock().unwrap();
                g.subscribers.retain(|tx| tx.send(ev.clone()).is_ok());
            }
            Err(e) => log::warn!("[auto-title {}] {}", tid, e),
        }
    });
}
```
This means `spawn_stdout_proxy` needs `store` and `cfg` in scope.
Update `spawn_worker` to capture and forward them (clone the `Arc`s).

`handle_auto_title` (existing) gets the same broadcast call after
successfully writing the title — refactor the broadcast block into a
helper `lifecycle::broadcast_meta_update(tree_id, title)`.

Tests to add:
- `test_auto_title_broadcasts_meta_update` — synthesize a worker entry,
  subscribe, force-fire the auto-title branch with a mock provider,
  assert the subscriber receives `MetaUpdate{title: Some(...)}`

Do not modify: agent-core/agent.rs (the `auto_title` function already
exists), tools, CLI.

**Verify:** Create a fresh tree, send one message; the CLI prints a
`meta_update` event with the title after the turn completes.

**Notes:**
_(fill in on completion)_

---

### Step 7 — Unified binary + remove old code

- [ ]

**Goal:** Remove the old thread-based agent lifecycle and the SSE path.

**Spec details:**

Files deleted (or stripped):
- `agent-server/src/lifecycle.rs` — remove `AgentHandle`, `ACTIVE_AGENTS`,
  `spawn` (thread-based), `stop` (old), `send_message`, `get_handle`,
  the bridge thread, and the SSE-related helpers. Keep only the worker
  path added in 3b/3c
- `agent-server/src/routes.rs` — remove `handle_sse_stream`, `SseUpgrade`,
  and `handle_send_message`. Remove `POST /trees/{id}/message` and
  `GET /trees/{id}/stream` from the dispatcher
- `agent-cli/src/client.rs` — remove `SseEventStream` and the
  `send_message` HTTP path if no longer used
- `agent-server/src/main.rs` — drop any old-lifecycle wiring

Verify nothing else imports the removed items: `cargo build --workspace`
and search for any leftover references with `grep`.

Do not modify: `agent-core/src/agent.rs` (the agent loop is unchanged —
only its host changes), tools, hooks, store.

**Verify:** `cargo clippy --workspace -- -D warnings` clean; `cargo test
--workspace` green; full interactive session and one-shot `msg` over WS
still work.

**Notes:**
_(fill in on completion)_

---

### Step 8 — Per-tree sandbox config (schema + editing surface)

- [ ]

**Goal:** Plumb the `TreeSandbox` field added in Step 1 through CLI and
HTTP. Validate `repo_path`. Still no bwrap enforcement.

**Spec details:**

Files modified:
- `agent-server/src/routes.rs` — `CreateTreeBody` and `UpdateTreeBody`
  accept `sandbox`; validate `repo_path` on create
- `agent-cli/src/main.rs` — `create` subcommand accepts new flags
- `agent-core/src/types.rs` — already updated in Step 1 (TreeSandbox);
  add a public `validate_repo_path` function

New CLI flags on `create`:
```
--writable <PATH>        # repeatable; in addition to repo_path
--no-net                 # sets sandbox.network = Some(false)
--net                    # sets sandbox.network = Some(true)
--hide <PATH>            # repeatable
--unhide <PATH>          # repeatable
```
Use clap derive's `Vec<PathBuf>` with `action = clap::ArgAction::Append`
for the repeatable flags. `--no-net` and `--net` are mutually exclusive
flags resolving to `Option<bool>`.

`validate_repo_path` (in `agent-core/src/types.rs` or a new
`agent-core/src/sandbox.rs`):
```rust
pub fn validate_repo_path(
    repo_path: &std::path::Path,
    defaults_hide: &[PathBuf],
    sandbox: &TreeSandbox,
) -> Result<PathBuf, String> {
    let canon = std::fs::canonicalize(repo_path)
        .map_err(|e| format!("canonicalize {:?}: {}", repo_path, e))?;
    if !canon.is_dir() { return Err(format!("{:?} is not a directory", canon)); }

    let home = dirs_home()?;
    let banned: &[&Path] = &[
        Path::new("/"),
        &home,
        &home.join(".agent"),
        &home.join(".config/agent"),
    ];
    if banned.iter().any(|b| canon == **b) {
        return Err(format!("repo_path {:?} is not allowed", canon));
    }

    // Reject if canon == any default-hide path (after applying unhide)
    let effective_hide: Vec<PathBuf> = defaults_hide.iter()
        .chain(sandbox.hide.iter())
        .filter(|p| !sandbox.unhide.contains(p))
        .map(|p| expand_tilde(p))
        .collect();
    if effective_hide.iter().any(|h| canon == *h || canon.starts_with(h)) {
        return Err(format!("repo_path {:?} overlaps a hidden directory", canon));
    }
    Ok(canon)
}
```
`dirs_home()` and `expand_tilde()` are small helpers — keep them in
the same file.

`CreateTreeBody` (in routes.rs) gains `sandbox: Option<TreeSandbox>`.
`handle_create_tree` calls `validate_repo_path` after canonicalizing
`repo_path`; returns 400 with the error string on failure.

`UpdateTreeBody` (in routes.rs) gains `sandbox: Option<TreeSandbox>`.

Tests to add:
- `test_validate_repo_path_rejects_home` — `validate_repo_path(&home, ...)` errors
- `test_validate_repo_path_rejects_hidden_overlap` — repo at
  `~/.ssh/something` errors when `~/.ssh` is in defaults_hide
- `test_validate_repo_path_accepts_normal` — `~/Code/repo` ok

Do not modify: bwrap code (doesn't exist yet), worker, agent loop.

**Verify:** `agent create --writable ~/Code/foo --no-net "title"` →
returns 201 with sandbox echoed in the meta response. `agent create ...
--repo-path /` → returns 400 with a useful message.

**Notes:**
_(fill in on completion)_

---

### Step 9 — bubblewrap sandboxing

- [ ]

**Goal:** Workers actually run inside bwrap configured from `TreeSandbox`
+ `[sandbox.defaults]`.

**Spec details:**

Files modified:
- `agent-core/src/config.rs` — add `SandboxConfig` with `enabled`,
  `bwrap_path`, and `defaults: SandboxDefaults { hide: Vec<PathBuf> }`
- `agent-server/src/lifecycle.rs` — `build_bwrap_argv()`,
  invoke `bwrap` instead of the exe directly when sandbox is enabled
- `config.toml` (the in-tree example) — show `[sandbox]` and
  `[sandbox.defaults]` examples

Config additions:
```rust
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub bwrap_path: Option<PathBuf>,
    pub defaults: SandboxDefaults,
}

#[derive(Clone, Debug, Default)]
pub struct SandboxDefaults {
    pub hide: Vec<PathBuf>,
}
```
Defaults: `enabled = true`; `bwrap_path = None` (probed at startup —
log "sandboxing active" if found, else log "bwrap not found, workers
will run unsandboxed"); `defaults.hide` = the curated list from the
"Default credential blocklist" section above.

`build_bwrap_argv` (in `lifecycle.rs`):
```rust
fn build_bwrap_argv(
    exe: &Path,
    tree_id: &str,
    meta: &TreeMeta,
    cfg: &Config,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    // structural mounts
    args.extend(["--ro-bind", "/", "/"].iter().map(OsString::from));
    args.extend(["--dev", "/dev"].iter().map(OsString::from));
    args.extend(["--proc", "/proc"].iter().map(OsString::from));
    args.extend(["--tmpfs", "/tmp"].iter().map(OsString::from));

    // The tree's own data dir + repo + config
    let store_dir = agent_dir().join("trees").join(tree_id);
    args.extend(["--bind".into(), store_dir.clone().into(), store_dir.into()]);
    if let Some(repo) = &meta.repo_path {
        args.extend(["--bind".into(), repo.clone().into(), repo.clone().into()]);
    }
    let config_path = agent_dir().join("config.toml");
    args.extend(["--ro-bind".into(), config_path.clone().into(), config_path.into()]);
    args.extend(["--ro-bind".into(), exe.to_path_buf().into(), exe.to_path_buf().into()]);

    // Per-tree extra writables
    for p in &meta.sandbox.writable {
        let p = expand_tilde(p);
        if p.exists() {
            args.extend(["--bind".into(), p.clone().into(), p.into()]);
        }
    }

    // Hide = defaults + sandbox.hide minus sandbox.unhide
    let mut hide_set: BTreeSet<PathBuf> = cfg.sandbox.defaults.hide.iter().cloned().collect();
    hide_set.extend(meta.sandbox.hide.iter().cloned());
    for u in &meta.sandbox.unhide {
        hide_set.remove(u);
    }
    for p in hide_set {
        let p = expand_tilde(&p);
        if p.exists() {
            args.extend(["--tmpfs".into(), p.into()]);
        }
    }

    // Namespace + network
    args.push("--unshare-all".into());
    let allow_net = meta.sandbox.network.unwrap_or(true);
    if allow_net { args.push("--share-net".into()); }
    args.push("--new-session".into());
    args.push("--die-with-parent".into());

    // Worker command
    args.push("--".into());
    args.push(exe.to_path_buf().into());
    args.push("worker".into());
    args.push("--tree-id".into());
    args.push(tree_id.into());
    args.push("--config".into());
    args.push(agent_dir().join("config.toml").into());

    args
}
```

`spawn_worker` (modified) — if `cfg.sandbox.enabled` and bwrap found:
`Command::new(bwrap_path).args(build_bwrap_argv(...))`. Otherwise:
`Command::new(exe).arg("worker").arg("--tree-id").arg(tree_id).arg("--config").arg(config_path)`
(unchanged direct spawn).

Sensitive env: pass `LLM_API_KEY` (and any others that contain secrets)
via `Command::env(...)` so they reach the worker. bwrap inherits the
parent env by default; if you want to be strict, add `--clearenv` and
pass only allowlisted vars explicitly — for now, inherit.

Tests to add:
- `test_build_bwrap_argv_basic` — call `build_bwrap_argv` with a fixed
  `TreeMeta` + `Config` and assert specific argv entries appear in the
  expected order
- `test_build_bwrap_argv_no_net` — assert `--share-net` is absent when
  `sandbox.network = Some(false)`
- `test_build_bwrap_argv_unhide` — assert a path in `defaults.hide` that
  is also in `sandbox.unhide` does not produce a `--tmpfs` arg

Do not modify: agent-core/agent.rs, tools, hooks.

**Verify:**
1. With `sandbox.enabled = true` and a tree whose `repo_path` is
   `~/Code/foo`: in a session, run `bash` tool with `cat ~/.ssh/id_rsa`
   → "No such file or directory" (tmpfs over `~/.ssh`)
2. Same tree, `touch /etc/foo` → EROFS
3. `touch ~/Code/foo/bar` → succeeds
4. Tree with `sandbox.network = Some(false)`: `curl https://example.com`
   → network unreachable
5. `sandbox.enabled = false`: same commands behave as without bwrap

**Notes:**
_(fill in on completion)_

---

### Step 10 — End-to-end worker integration test

- [ ]

**Goal:** Single integration test that spawns a real worker and asserts
the protocol survives.

**Spec details:**

New file: `agent-worker/tests/end_to_end.rs`

The test uses a stub provider via env-var indirection: `AGENT_TEST_STUB=1`
causes `agent-core::provider::Provider::stream_chat` to short-circuit to
a canned response. Add this stub behind a `#[cfg(feature = "test-stub")]`
gate (or a runtime env check at the top of `stream_chat`) so it never
ships in release builds.

Test sketch:
```rust
#[test]
fn worker_round_trip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let agent_dir = tmp.path().to_path_buf();
    let tree_id = "test-tree-0001";

    // 1. Seed the store with a tree
    let store = agent_core::store::Store::new(agent_dir.clone());
    store.create_tree_file(tree_id, "model").unwrap();
    // ... write session_start, meta.json ...

    // 2. Spawn worker subprocess
    let exe = env!("CARGO_BIN_EXE_agent"); // requires the `agent` binary
    let mut child = std::process::Command::new(exe)
        .arg("worker")
        .arg("--tree-id").arg(tree_id)
        .arg("--config").arg(agent_dir.join("config.toml"))
        .env("AGENT_DIR", &agent_dir)
        .env("AGENT_TEST_STUB", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn().unwrap();

    // 3. Send a message
    use std::io::Write;
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"method":"message","params":{{"text":"hi"}}}}"#).unwrap();
    drop(stdin);

    // 4. Collect events from stdout
    use std::io::BufRead;
    let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let events: Vec<agent_core::types::ServerEvent> = stdout.lines()
        .flatten()
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();

    // 5. Assertions
    assert!(events.iter().any(|e| matches!(e, ServerEvent::TextChunk { .. })));
    assert!(events.iter().any(|e| matches!(e, ServerEvent::Done { .. })));

    // 6. Tree file contains the user + assistant messages
    let entries = store.read_all_entries(tree_id).unwrap();
    assert!(entries.iter().any(|e| matches!(e, Entry::Message { message, .. } if matches!(message.role, MessageRole::User))));

    let _ = child.wait();
}
```

If stubbing the provider is too invasive, alternative: spin up a tiny
HTTP server in the test that responds to `POST /v1/chat/completions`
with a canned SSE stream, and point `config.provider.base_url` at it.
Pick whichever is less code.

Do not modify: anything else.

**Verify:** `cargo test -p agent-worker --test end_to_end` passes.

**Notes:**
_(fill in on completion)_

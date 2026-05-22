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

### LspClient — post-edit diagnostics via Language Server Protocol

- [x] `agent-core/types.rs`: add `LspConfig`, `LspServerConfig`, `Diagnostic`, `DiagnosticSeverity`, `Range`, `Position`
- [x] `agent-core/config.rs`: add `pub lsp: LspConfig` to `Config`; parse `[lsp]` / `[[lsp.servers]]` in `apply_toml`
- [x] `agent-core/rpc.rs`: add `pub lsp: LspConfig` to `WorkerConfig`
- [x] `agent-server/lifecycle.rs`: populate `lsp: cfg.lsp.clone()` in the `WorkerConfig` sent at startup
- [x] `agent-worker/Cargo.toml`: add `lsp-types = "0.95"`
- [x] `agent-worker/src/tools/mod.rs`: add `ToolOutput` enum; add `resume` to `Tool` trait; add `lsp_dirty` and `lsp_clients` to `ToolContext`
- [x] `agent-worker/src/lsp_client.rs` (new): `LspClient` (fd-based, no reader thread), `LspWaitState`, `PendingLspTool`, `notify_saved`, `send_request`, `read_available`, `detect_language`, `default_server`, `binary_exists`
- [x] `agent-worker/src/lib.rs`: switch main loop from `BufReader::read_line` to `nix::poll`; handle LSP fd events, pending tool request resolution, and wait resolution
- [x] `agent-worker/src/lib.rs`: add `lsp_wait: Option<LspWaitState>` to `AgentState::Streaming`
- [x] `agent-worker/src/tools/edit.rs` + `write.rs`: push written path onto `ctx.lsp_dirty` on success
- [x] `agent-worker/src/turn.rs`: after tool dispatch call `notify_saved` per dirty file, set `lsp_wait`, return without calling `send_llm_request`; add `format_diagnostics`
- [ ] Integration smoke test (manual): rust-analyzer enabled, edit a Rust file with a type error, confirm diagnostics appear in the next LLM turn

**Goal:** After each tool turn that writes files, the worker notifies language server(s) and enters an LSP-wait state. The main poll loop — cooperative with stdin — drives the wait: it reads LSP frames as they arrive, resets the silence timer, and fires the next LLM request once quiet. No blocking, no reader threads, no pipe protocol changes, no server-side involvement.

---

#### Architecture

LSP clients live entirely in `agent-worker`. The worker spawns language server subprocesses and communicates over their stdin/stdout. The worker's main loop switches from a blocking `BufReader::read_line` to a `nix::poll` loop that watches both stdin and all active LSP stdout fds — the same model the server already uses for its handlers. This makes LSP fully cooperative: `Stop` commands and LLM chunks are processed immediately even while waiting for diagnostics.

```
                  ┌─ poll ─┐
agent-worker      │ stdin  │ ◀── PipeIn (server)
(nix::poll loop)  │ lsp fd │ ◀── publishDiagnostics (rust-analyzer)
                  └────────┘
                       │
                       ▼ notify_saved
               rust-analyzer stdin
```

Do NOT add LSP to `agent-server`. Do NOT add `PipeOut::LspRequest` or `PipeIn::LspResponse`.

---

#### `agent-core/src/types.rs` additions

```rust
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
    /// Hard deadline: give up if total wait exceeds this.
    #[serde(default = "default_lsp_timeout")]
    pub timeout_ms: u64,
    /// Silence window: stop waiting once no new publishDiagnostics arrives for this long.
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
```

`LspConfig` and `LspServerConfig` live in `types.rs` (not `config.rs`) because `WorkerConfig` in `rpc.rs` needs to embed `LspConfig`, and `rpc.rs` already imports from `types`.

---

#### `agent-core/src/config.rs`

Add `pub lsp: LspConfig` to `Config` with `Default::default()` in `Config::default()`.

In `apply_toml`:
```toml
[lsp]
enabled = true

[[lsp.servers]]
language = "rust"
command = "rust-analyzer"
timeout_ms = 5000
silence_ms = 500

[[lsp.servers]]
language = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
timeout_ms = 8000
silence_ms = 500
```

---

#### `agent-core/src/rpc.rs`

Add to `WorkerConfig`:
```rust
#[serde(default)]
pub lsp: agent_core::types::LspConfig,
```

---

#### `agent-server/src/lifecycle.rs`

In `spawn_worker`, add `lsp: cfg.lsp.clone()` to the `WorkerConfig` literal sent at startup.

---

#### `agent-worker/src/lsp_client.rs` (new file)

```rust
pub struct LspFileResult {
    pub path: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Tracks a tool that fired an LSP request and is waiting for the response.
pub struct PendingLspTool {
    pub request_id: u64,
    pub lang_id: String,
    pub tool_name: String,           // used to find the right Tool impl for resume()
    pub tool_call_id: String,        // original LLM tool_call_id for the result message
}

pub struct LspWaitState {
    pub deadline: Instant,
    pub silence_until: Instant,
    pub silence_ms: u64,
    /// Tools that fired LSP requests this round and are awaiting responses.
    /// resolve_lsp_wait only fires once this is empty (or deadline passes).
    pub pending_tool_requests: Vec<PendingLspTool>,
}

pub struct LspClient {
    stdin: std::process::ChildStdin,
    pub stdout_fd: RawFd,
    _child: std::process::Child,      // kept alive; dropped when LspClient is dropped
    read_buf: Vec<u8>,
    next_id: u64,
    opened: std::collections::HashSet<lsp_types::Url>,
    // Full-replacement diagnostics state; updated on every publishDiagnostics notification.
    // INTENTIONAL: stores the complete workspace picture, not just files we saved,
    // so cross-file effects (broken callers, etc.) are captured automatically.
    pub diagnostics: std::collections::HashMap<lsp_types::Url, Vec<Diagnostic>>,
    // Responses to active requests, keyed by request id.
    // read_available() populates this; send_request callers drain it via poll loop.
    pub pending_responses: std::collections::HashMap<u64, serde_json::Value>,
    root_uri: String,
    pub lang_id: String,
}
```

`spawn(lang_id, command, args, root_uri, timeout_ms) -> Result<LspClient, String>`:
1. `Command::new(command).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()`
2. Extract `stdout_fd` via `child.stdout.as_ref().unwrap().as_raw_fd()`
3. Set stdout fd non-blocking: `fcntl(stdout_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))`
4. Send `initialize` request (processId, rootUri, minimal capabilities)
5. Block on an inline `nix::poll` loop reading from `stdout_fd` until the `initializeResult` response arrives (with timeout). This is the one synchronous wait — it happens once at first use and LSP servers respond within milliseconds.
6. Send `initialized` notification
7. Return `LspClient` — stdout fd is now non-blocking and ready for the main poll loop

If `spawn` fails (binary not found, timeout, error) return `Err` — caller skips that language silently.

`notify_saved(&mut self, path: &Path)`:
1. Convert `path` to `lsp_types::Url`, read content with `std::fs::read_to_string`
2. If not in `self.opened`: send `textDocument/didOpen` with `languageId` and content; insert into `self.opened`
3. Else: send `textDocument/didChange` (full sync, kind=1) then `textDocument/didSave`
4. Writes go to `self.stdin` — the pipe is buffered by the OS; no blocking.

`send_request(&mut self, method: &str, params: serde_json::Value) -> u64`:
Write a JSON-RPC request frame with a fresh `self.next_id` to `self.stdin`. Return the ID. Non-blocking — the OS pipe buffer absorbs the write. The caller adds a `PendingLspTool` to `LspWaitState`; the response arrives later via `read_available`.

`read_available(&mut self) -> bool`:

Called by the main poll loop when `stdout_fd` is ready. Returns `true` if any new frames were parsed (diagnostic update or response arrived).

```
loop:
    match nix::unistd::read(stdout_fd, &mut tmp):
        Ok(0) => break   // EOF — LSP server exited
        Ok(n) => read_buf.extend(&tmp[..n])
        Err(EAGAIN | EWOULDBLOCK) => break
        Err(e) => { warn!; break }

// Parse all complete Content-Length frames from read_buf:
//   scan for "\r\n\r\n", extract Content-Length: N, consume N bytes as JSON
// For each frame:
//   if "method" == "textDocument/publishDiagnostics":
//       url = params.uri
//       diags = convert lsp_types::Diagnostic → Diagnostic
//       self.diagnostics.insert(url, diags)   // full-replacement per file
//       updated = true
//   elif "id" present (response to a request):
//       self.pending_responses.insert(id, frame)
//       updated = true
//   else: ignore (progress notifications, etc.)
return updated
```

`all_diagnostics(&self) -> Vec<LspFileResult>`:
Flatten `self.diagnostics` into a `Vec`, excluding files with empty lists. Called once the silence window expires.

JSON-RPC framing:
- **Write**: `format!("Content-Length: {}\r\n\r\n{}", body.len(), body)` written to `self.stdin`
- **Read**: incremental in `read_buf` — scan for `b"\r\n\r\n"`, parse `Content-Length: N`, consume exactly N body bytes, leave remainder in `read_buf`

`detect_language(path: &Path) -> Option<&'static str>`:

| Extension(s) | `lang_id` |
|---|---|
| `.rs` | `rust` |
| `.ts`, `.tsx` | `typescript` |
| `.js`, `.jsx`, `.mjs` | `javascript` |
| `.py` | `python` |
| `.go` | `go` |
| `.c`, `.h` | `c` |
| `.cpp`, `.cc`, `.cxx`, `.hpp` | `cpp` |

`default_server(lang_id: &str) -> Option<LspServerConfig>`:

| `lang_id` | `command` | `args` |
|---|---|---|
| `rust` | `rust-analyzer` | `[]` |
| `typescript`, `javascript` | `typescript-language-server` | `["--stdio"]` |
| `python` | `pylsp` | `[]` |
| `go` | `gopls` | `[]` |
| `c`, `cpp` | `clangd` | `[]` |

`binary_exists(cmd: &str) -> bool`: attempt `Command::new(cmd).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).spawn()`, return `true` if `spawn()` succeeds. On spawn failure with `ErrorKind::NotFound`, return `false`.

Resolution order used in the worker when choosing a server config for a given `lang_id`:
1. Explicit entry in `cfg.lsp.servers` with matching `language`
2. `default_server(lang_id)`
3. Skip (log `warn!`) if binary not found or no config

---

#### `agent-worker/src/lib.rs`

**`AgentState::Streaming`** gains one field:
```rust
lsp_wait: Option<LspWaitState>,
```
Initialized to `None` in `new_streaming`. All existing destructuring sites must add `lsp_wait: _` or `lsp_wait`.

**Main loop** switches from `BufReader::read_line` to `nix::poll`. New structure:

```rust
let stdin_fd = std::io::stdin().as_raw_fd();
// Set stdin non-blocking
nix::fcntl::fcntl(stdin_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();

let mut stdin_buf: Vec<u8> = Vec::new();
// lsp_clients lives in ctx.lsp_clients so tools can reach them directly

loop {
    // Compute poll timeout
    let timeout_ms: i32 = match &state {
        AgentState::Streaming { lsp_wait: Some(wait), .. } => {
            let until = wait.silence_until.min(wait.deadline);
            until.saturating_duration_since(Instant::now()).as_millis()
                .try_into().unwrap_or(0)
        }
        _ => -1,  // block indefinitely until stdin has data
    };

    // Build pollfds: stdin first, then one per LSP client
    let mut pollfds: Vec<PollFd> = std::iter::once(
        PollFd::new(unsafe { BorrowedFd::borrow_raw(stdin_fd) }, PollFlags::POLLIN)
    ).chain(
        ctx.lsp_clients.values().map(|c|
            PollFd::new(unsafe { BorrowedFd::borrow_raw(c.stdout_fd) }, PollFlags::POLLIN)
        )
    ).collect();

    nix::poll::poll(&mut pollfds, timeout_ms).ok();

    // Drain stdin
    if pollfds[0].revents().map_or(false, |r| r.contains(PollFlags::POLLIN)) {
        let msgs = drain_stdin_pipe(stdin_fd, &mut stdin_buf);
        for msg in msgs {
            dispatch_pipe_in(msg, &mut state, &tree_id, &store, &session_cfg,
                             &tools, &mut ctx, &mut out, &config.lsp);
        }
    }

    // Drain LSP fds
    let lang_ids: Vec<String> = ctx.lsp_clients.keys().cloned().collect();
    for (i, lang_id) in lang_ids.iter().enumerate() {
        if pollfds[i + 1].revents().map_or(false, |r| r.contains(PollFlags::POLLIN)) {
            let updated = ctx.lsp_clients.get_mut(lang_id).unwrap().read_available();
            if updated {
                if let AgentState::Streaming { lsp_wait: Some(ref mut wait), ref mut messages, .. } = state {
                    wait.silence_until = Instant::now() + Duration::from_millis(wait.silence_ms);
                    // Resolve any pending tool requests whose responses have arrived.
                    // Two-pass to avoid a double mutable borrow of ctx: first extract
                    // responses from lsp_clients (releasing that borrow), then call resume.
                    let mut resolved_indices: Vec<usize> = Vec::new();
                    let mut resolved_responses: Vec<(serde_json::Value, String, String)> = Vec::new();
                    for (i, pending) in wait.pending_tool_requests.iter().enumerate() {
                        if let Some(client) = ctx.lsp_clients.get_mut(&pending.lang_id) {
                            if let Some(response) = client.pending_responses.remove(&pending.request_id) {
                                resolved_indices.push(i);
                                resolved_responses.push((response, pending.tool_name.clone(), pending.tool_call_id.clone()));
                            }
                        }
                    }
                    for (response, tool_name, tool_call_id) in resolved_responses {
                        if let Some(tool) = tools.iter().find(|t| t.name() == tool_name) {
                            let result = tool.resume(response, &mut ctx);
                            messages.push(tool_result_message(&tool_call_id, result));
                        }
                    }
                    for i in resolved_indices.into_iter().rev() {
                        wait.pending_tool_requests.swap_remove(i);
                    }
                }
            }
        }
    }

    // Resolve LSP wait if all tool requests resolved AND silence/deadline passed
    if let AgentState::Streaming { lsp_wait: Some(ref wait), .. } = state {
        let now = Instant::now();
        let tools_done = wait.pending_tool_requests.is_empty();
        if tools_done && (now >= wait.silence_until || now >= wait.deadline) {
            state = resolve_lsp_wait(state, &ctx.lsp_clients, &mut out, &tools);
        } else if !tools_done && now >= wait.deadline {
            // Deadline passed with unresolved tool requests — synthesize timeout errors
            state = resolve_lsp_wait_with_timeout(state, &mut ctx, &mut out, &tools);
        }
    }
}
```

`drain_stdin_pipe(fd, buf) -> Vec<PipeIn>`: read available bytes with `nix::unistd::read` into `buf`, catching `EAGAIN`/`EWOULDBLOCK`; extract `\n`-terminated lines; parse each as `PipeIn`. Return parsed messages.

`dispatch_pipe_in(...)`: the body of the existing `match msg { ... }` block, extracted into a function so the poll loop stays readable.

`resolve_lsp_wait(state, lsp_clients, out, tools) -> AgentState`: collect `all_diagnostics()` from all clients, append diagnostic message if non-empty, call `send_llm_request`, return `AgentState::Streaming { lsp_wait: None, ... }`.

`resolve_lsp_wait_with_timeout(state, ctx, out, tools) -> AgentState`: for each entry still in `pending_tool_requests`, append a `tool_result_message` with an error string `"LSP request timed out"`, then call `resolve_lsp_wait`.

```rust
fn resolve_lsp_wait(
    state: AgentState,
    lsp_clients: &HashMap<String, LspClient>,
    out: &mut BufWriter<Stdout>,
    tools: &[Box<dyn Tool>],
) -> AgentState {
    let AgentState::Streaming {
        mut messages, leaf_id, tool_call_round,
        tool_calls_this_turn, consecutive_failures, ..
    } = state else { return state };

    let results: Vec<LspFileResult> = lsp_clients.values()
        .flat_map(|c| c.all_diagnostics())
        .collect();
    if !results.is_empty() {
        messages.push(Message {
            role: MessageRole::Tool,
            content: MessageContent::Text(format_diagnostics(&results)),
            tool_call_id: Some("lsp_diagnostics".into()),
            tool_name: Some("lsp_diagnostics".into()),
            ..Default::default()
        });
    }
    let definitions = tools.iter().map(|t| t.definition()).collect();
    send_llm_request(out, messages.clone(), definitions);
    AgentState::new_streaming(messages, leaf_id, tool_call_round, tool_calls_this_turn, consecutive_failures)
}
```

---

#### `agent-worker/src/tools/mod.rs`

**`ToolOutput`** — new return type for `Tool::execute`:
```rust
pub enum ToolOutput {
    Done(Result<String, String>),
    /// Tool fired an LSP request and needs the response before it can produce a result.
    /// The main poll loop will call tool.resume() when the response arrives.
    PendingLsp { request_id: u64, lang_id: String },
}
```

**`Tool` trait** gains:
```rust
fn execute(&self, args: serde_json::Value, ctx: &mut ToolContext) -> ToolOutput;

fn resume(
    &self,
    _response: serde_json::Value,
    _ctx: &mut ToolContext,
) -> Result<String, String> {
    unreachable!("tool '{}' does not implement resume", self.name())
}
```

All existing tools change their `execute` return from `Result<String, String>` to `ToolOutput::Done(...)`. No other changes needed to existing tools.

**`ToolContext`** gains:
```rust
pub lsp_dirty: Vec<PathBuf>,
pub lsp_clients: HashMap<String, LspClient>,
```

Both initialized to empty. Tools that make active LSP requests call `ctx.lsp_clients.get_mut(lang_id).map(|c| c.send_request(...))` directly.

INTENTIONAL: `lsp_clients` lives in `ToolContext` (not `lib.rs` locals) so that future LSP-backed tools can reach language servers without any extra parameter threading.

---

#### `agent-worker/src/tools/edit.rs` and `write.rs`

After a successful `fs::write`, push the resolved path onto `ctx.lsp_dirty`. Do not push on error.

---

#### `agent-worker/src/turn.rs`

Signature change: add `lsp_cfg: &LspConfig` to `finish_response`. `lsp_clients` is accessed via `ctx.lsp_clients`; no extra parameter needed.

In the `StopReason::ToolCalls` branch, the tool dispatch loop changes its inner result handling:

```rust
// existing: let result = tool.execute(&args, ctx);  // was Result<String,String>
match tool.execute(args, ctx) {
    ToolOutput::Done(result) => {
        messages.push(tool_result_message(&call.id, result));
    }
    ToolOutput::PendingLsp { request_id, lang_id } => {
        pending_lsp_tools.push(PendingLspTool {
            request_id, lang_id,
            tool_name: tool.name().to_string(),
            tool_call_id: call.id.clone(),
        });
        // No message appended yet — added by poll loop when response arrives
    }
}
```

After all tool calls processed, replace the existing `send_llm_request` call with:

```rust
let dirty = std::mem::take(&mut ctx.lsp_dirty);
let needs_lsp_wait = lsp_cfg.enabled && (!dirty.is_empty() || !pending_lsp_tools.is_empty());
if needs_lsp_wait {
    let (timeout_ms, silence_ms) = notify_lsp_saves(&mut ctx, lsp_cfg, &dirty, &pending_lsp_tools);
    return AgentState::Streaming {
        messages, leaf_id,
        response_text: String::new(), in_thinking: false,
        tool_calls_buf: vec![], finish_reason: None,
        tool_call_round, tool_calls_this_turn, consecutive_failures,
        lsp_wait: Some(LspWaitState {
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            silence_until: Instant::now() + Duration::from_millis(silence_ms),
            silence_ms,
            pending_tool_requests: pending_lsp_tools,
        }),
    };
}
send_llm_request(out, messages.clone(), definitions);
AgentState::new_streaming(messages, leaf_id, tool_call_round, tool_calls_this_turn, consecutive_failures)
```

`notify_lsp_saves(ctx, cfg, dirty, pending_tools) -> (timeout_ms, silence_ms)`:
1. Group `dirty` paths by `detect_language`
2. For each `(lang_id, paths)` group: resolve server config; get or spawn `LspClient` in `ctx.lsp_clients`; call `client.notify_saved(path)` for each path; collect that config's `timeout_ms`/`silence_ms`
3. Also include the configs for any `lang_id` referenced in `pending_tools` (those clients already exist)
4. Return the max `timeout_ms` and `silence_ms` across all collected configs, falling back to `(5000, 500)` if none resolved (shouldn't happen if called only when `needs_lsp_wait`)

`format_diagnostics(results: &[LspFileResult]) -> String`: sort errors before warnings, format as `path:line:col: severity[code]: message`. Return `"No diagnostics."` if all lists are empty.

---

#### Do not modify

- `agent-server/` — no LSP involvement whatsoever
- `agent-core/src/rpc.rs` `PipeOut`/`PipeIn` enums — no new variants

---

#### Dependency additions

- `agent-worker/Cargo.toml`: `lsp-types = "0.95"`; update `nix` features to add `poll` and `fcntl` (currently only `signal, process`)
- `agent-core/Cargo.toml`: no new deps
- `agent-server/Cargo.toml`: no new deps

---

**Verify:**
```
cargo build --workspace
cargo test --workspace
# Manual smoke test:
# 1. Set lsp.enabled = true in ~/.agent/config.toml
# 2. Open an agent tree on a Rust project
# 3. Ask the agent to introduce a type error in a .rs file
# 4. Confirm the next LLM turn includes the rust-analyzer error in context
# 5. Ask the agent to fix it; confirm diagnostics are empty in the following turn
```


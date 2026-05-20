# Plan

Steps + Notes for ongoing work on the agent server. Architecture overview and
"how things fit together" live in `AGENTS.md`; troubleshooting recipes in
`DEBUG.md`. This file is only step specs (what to build next) and the notes we
write under them when a step is done.

---

## Step template

```
### Step N — Title

- [ ] todo / - [x] done

**Goal:** one or two sentences.

**Spec details:** file paths, signatures, tests, do-not-modify list.

**Verify:** commands that prove it works.

**Notes:** (filled in on completion)
- Created / Modified: ...
- Deviation: chose X over Y because ...
- Verified: cargo test --workspace → N passed
```

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

## Future ideas

Small things worth doing eventually; promote to a numbered Step when picked up.

- **File-change awareness.** Watch the repo directory with the `notify` crate.
  Track files the agent reads each turn; before the next LLM call, inject a
  system message listing any of those files that were modified externally.
  Same event stream can later push `FileChanged` events to a PWA. The
  `ServerEvent::FileChanged { path, kind }` variant already exists for this.
- **Queued input while the agent is working.** Buffer user input typed during
  streaming; flush on Enter as a pending message that sends once the current
  turn ends. (Cancellation in Step 1 covers the immediate-stop case; this is
  the friendlier "I have a follow-up" case.)
- **5xx retry with exponential backoff** in the provider client. Currently any
  provider error is fatal; three retries on transient 5xx would smooth over
  flaky upstream.
- **Turn timeout** (configurable, default ~300s) in the agent loop, alongside
  the existing `max_tool_calls_per_turn` guard.
- **Subtask spawning** — child trees linked via `parent_id`, so the agent can
  fork a sub-investigation that returns a summary into the parent.
- **Autonomous ralph loop** — agent self-continues without user input on a
  cadence.
- **PWA frontend** — browser client over WS, voice input via Web Speech API.
- **Provider abstraction** beyond the current single-config struct (Anthropic,
  OpenAI, local OpenAI-compatible all supported via base_url today, but a
  trait would let us add response-shape adapters).

---

## Step 1 — Esc cancels the current turn

- [x] done

**Goal:** Pressing Esc in the interactive TUI immediately cancels the running
turn — including a long-running `bash` tool call and an in-progress LLM
stream — and shows visible feedback that it happened. The worker stays alive;
the next message in the same tree continues normally.

### Current behaviour (what to fix)

- The CLI's `process_message` (`agent-cli/src/interactive.rs:451`) blocks on
  `session.next_event()` for the entire turn. No keys are read during
  streaming, so Esc can't be observed.
- `/stop` at the prompt calls `client.stop_agent()` → server →
  `WsCommand::Stop` to the worker. The worker sets an `AtomicBool` AND pushes
  `AgentInput::Stop` onto the input channel (`agent-worker/src/lib.rs:42-45`).
- The atomic is checked at the outer `'turn` loop and between streamed SSE
  chunks (`agent-core/src/agent.rs:528,548`). That kills the LLM stream
  promptly but does **not** kill an executing tool — `Tool::execute()` doesn't
  receive the stop flag, so a `bash` running `sleep 60` finishes its full
  timeout.
- On stop mid-stream the agent `break 'turn`s past the `Done` emit
  (`agent.rs:548-550`), so the CLI never sees an end event and is left waiting
  on the WS read.
- `AgentInput::Stop` arriving on the input channel makes the outer `'main`
  loop `break` (`agent.rs:398-401`), terminating the agent thread. Result:
  worker exits, next message has to respawn a worker. We want cancel to
  *interrupt the turn*, not the worker.
- `render_done` (`interactive.rs:155`) maps the provider's `"stop"` finish
  reason to green `✓ Done` — model-finished — and `"aborted"` to red
  `✖ Aborted`. We need a third path for user-initiated cancel.

### Spec details

#### 1. Plumb stop signal through the tool trait

File: `agent-core/src/tools/mod.rs`.

```rust
pub trait Tool: Send {
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, params: &serde_json::Value, stop: &AtomicBool) -> ToolResult;
}
```

Update every tool's `execute` to accept the parameter. Fast tools
(`read`, `write`, `edit`, `grep`, `find`, `ls`, `git`, `search`) ignore
it — they complete in milliseconds. `bash` uses it (next bullet).

The agent's `execute_tool()` helper (`agent-core/src/agent.rs`) forwards
the existing `stop: &Arc<AtomicBool>` it already holds.

#### 2. Bash: kill the process group on stop

File: `agent-core/src/tools/bash.rs:106-120` (the timeout thread).

The existing thread polls every 100ms for `done`. Also poll `stop` in the
same loop. On either fire, `killpg(SIGTERM)` then `killpg(SIGKILL)` after
500ms. Use a small `enum KillReason { Timeout, Cancelled }` so
`combine_output` can label the message (`[Command timed out after Ns]` vs
`[Command cancelled]`).

Tests (`#[cfg(test)] mod tests` in `bash.rs`):
- `test_bash_cancels_on_stop_flag` — spawn `BashTool::execute` running
  `sleep 30` in a thread; from the test thread, sleep 100ms then set the
  stop atomic; assert the call returns within ~1s with the cancel marker
  in `content` and an exit code reflecting termination.

#### 3. Agent: emit Done on cancel, don't kill the worker

File: `agent-core/src/agent.rs`.

- At the two `break 'turn` sites that fire when `stop` is set (lines ~548
  during streaming, ~528 at top of each round), emit
  `ServerEvent::Done { status: "cancelled".into() }` *before* breaking.
- Persist a synthetic assistant message containing whatever `response_text`
  was streamed so far, if non-empty — same path as the `"stop" | "length"`
  arm (`agent.rs:766-786`). The partial text is real model output; throwing
  it away loses context. (If empty, skip the persist.)
- Reset `stop.store(false, Ordering::Relaxed)` at the top of the outer
  `'main` loop, right after the `recv()` returns a `Message`. Without this,
  a cancel that arrives during the idle wait would instantly cancel the
  next turn.
- Change the outer `recv()` Stop arm:
  ```rust
  Ok(AgentInput::Stop) => {
      // Worker stays alive; cancel applies only to in-flight work,
      // which the atomic flag above already handled. Drain and wait.
      continue;
  }
  ```
  Worker exit now happens only via `Err(_)` (channel closed = stdin EOF =
  worker shutting down).

#### 4. CLI render: cancelled status

File: `agent-cli/src/interactive.rs:155`.

Add an arm:
```rust
"cancelled" => write!(out, "\r\n  {}✋{} Cancelled\r\n",
                     color::Fg(color::Yellow), style::Reset),
```

Tests (alongside the existing `render_done_*` tests):
- `test_render_done_cancelled` — `render_done(&mut buf, "cancelled")`
  contains `✋` and `Cancelled`, not `Done` or `Aborted`.

#### 5. CLI: poll stdin during streaming

File: `agent-cli/src/interactive.rs::process_message` and
`client::AgentSession`.

The streaming loop currently does a single blocking `ws.read()`. Make the
underlying socket non-blocking and poll both sources in a tight loop.

Add to `AgentSession`:
```rust
pub fn set_nonblocking(&mut self, nb: bool) -> Result<(), String>;
pub fn send_stop(&mut self) -> Result<(), String>;
pub fn try_next_event(&mut self) -> TryEvent;

pub enum TryEvent {
    Event(ServerEvent),
    WouldBlock,
    Closed,
    Err(String),
}
```

`set_nonblocking` reaches into the inner `MaybeTlsStream` and calls
`set_nonblocking(true)` on the underlying `TcpStream`. `try_next_event`
catches `io::ErrorKind::WouldBlock` from `tungstenite::Error::Io` and
returns `WouldBlock`; everything else maps as before.

Rewrite `process_message`:
```rust
fn process_message(server, tree_id, text, out, stop) -> Result<(), String> {
    let mut session = AgentSession::connect(server, tree_id)?;
    session.set_nonblocking(true)?;
    session.send_message(text)?;

    let mut state = RenderState::default();
    let mut keys = io::stdin().keys();  // already in raw mode
    let mut cancel_signalled = false;

    loop {
        // 1. drain events
        match session.try_next_event() {
            TryEvent::Event(ev) => {
                let done = matches!(&ev, ServerEvent::Done { .. });
                render_event(out, &ev, &mut state);
                if done { break; }
                continue;  // try for more without sleeping
            }
            TryEvent::Closed => break,
            TryEvent::Err(e) => { print_warning(out, &format!("ws: {}", e)); break; }
            TryEvent::WouldBlock => {}
        }

        // 2. check Ctrl-C from the outer signal handler
        if stop.load(Ordering::Relaxed) {
            write!(out, "\r\nInterrupted\r\n").ok();
            break;
        }

        // 3. peek stdin (also non-blocking — see below)
        if !cancel_signalled {
            if let Some(Ok(key)) = poll_key(&mut keys) {
                if matches!(key, Key::Esc | Key::Ctrl('c')) {
                    write!(out, "\r\n  {}⏸ Cancelling…{}\r\n",
                           color::Fg(color::Yellow), style::Reset).ok();
                    out.flush().ok();
                    let _ = session.send_stop();
                    cancel_signalled = true;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
```

`poll_key` is a small helper that returns `None` if stdin has no byte ready
(use `termion::async_stdin().keys()` for the streaming-phase keys iterator,
*not* the same `keys` instance the prompt uses — async_stdin doesn't block).
Create it once at the top of `process_message` and drop it on return so the
prompt's blocking `stdin().keys()` resumes afterwards.

`send_stop` on `AgentSession`:
```rust
pub fn send_stop(&mut self) -> Result<(), String> {
    let s = serde_json::to_string(&WsCommand::Stop).map_err(|e| e.to_string())?;
    self.ws.send(tungstenite::Message::Text(s)).map_err(|e| e.to_string())
}
```

Tests (`#[cfg(test)] mod tests` in `client.rs`):
- `test_wscommand_stop_serializes` — `serde_json::to_string(&WsCommand::Stop)`
  equals `{"method":"stop"}`. Guards against accidental rename of the
  serde discriminant.

### Do not modify

- The `WsCommand` wire shape (server already understands `{"method":"stop"}`).
- `lifecycle.rs::worker_stop` — keep the HTTP `/stop` route too; some clients
  use it. The Esc path uses the WS frame directly, which is faster.
- bwrap argv, sandbox config, store layout.

### Verify

- `cargo test --workspace` — all existing tests still pass, three new tests
  pass.
- `cargo clippy --workspace` — no new warnings.
- Manual:
   1. Send a message that triggers a long bash call
      (`"run sleep 30 then say hi"`).
   2. Press Esc within 1s of the bash starting.
   3. Observe `⏸ Cancelling…` appears immediately, then `✋ Cancelled`
      within ~1s. The `sleep` process exits (verify with `pgrep -f sleep`).
   4. Send a second message in the same tree; agent responds normally
      (worker did not die).
- Manual: send a message and press Esc *during* the LLM stream (not in
  bash). Same outcome — `✋ Cancelled` within a chunk boundary
  (~few hundred ms).

**Notes:** _(filled on completion)_
- Created / Modified: `agent-core/src/tools/mod.rs` (trait signature + Arc import),
  `agent-core/src/tools/bash.rs` (KillReason, stop polling in thread, test),
  `agent-core/src/tools/edit.rs`, `find.rs`, `git.rs`, `grep.rs`, `ls.rs`,
  `read.rs`, `search.rs`, `write.rs` (stop param on execute impl + test calls),
  `agent-core/src/agent.rs` (execute_tool forwards stop, cancel Done at break sites,
  partial text persistence, stop reset, Stop arm → continue),
  `agent-cli/src/client.rs` (TryEvent, set_nonblocking, send_stop, try_next_event, test),
  `agent-cli/src/interactive.rs` (cancelled arm in render_done, poll_key, rewritten process_message, test).
- Deviation: trait uses `&Arc<AtomicBool>` instead of `&AtomicBool` so the bash
  thread can clone the Arc. Test call sites pass `&Arc::new(AtomicBool::new(false))`.
- Verified: `cargo test --workspace` → 122 passed, `cargo clippy --workspace` → only pre-existing warnings.

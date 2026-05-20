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

**Notes:**
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

---

## Step 2 — Fix bash cancellation latency

- [x] Reduce stop-poll interval in bash timeout thread
- [ ] Verify cancellation feels immediate

**Goal:** Make pressing Esc during a bash tool call feel instantaneous (< ~20ms to
SIGTERM) rather than sluggish. Currently worst-case is ~130ms because the timeout
thread wakes every 100ms.

**Root cause:** `agent-core/src/tools/bash.rs:127` — the timeout/cancellation
thread sleeps 100ms between stop-flag checks:

```rust
for _ in 0..(timeout_secs * 10) {   // ← 10 iters/sec because of 100ms sleep
    if done_clone.load(…) { return; }
    if stop_for_thread.load(…) { … kill … return; }
    std::thread::sleep(Duration::from_millis(100));
}
```

The fix is to tighten the loop. We are already polling an `AtomicBool` (no syscall)
so 5ms per iteration costs nothing — the thread is blocked 99.5% of the time.

**Spec details:**

File: `agent-core/src/tools/bash.rs`.

Change the loop structure so the sleep is 5ms and the iteration count scales
accordingly:

```rust
let poll_ms: u64 = 5;
let iterations = (timeout_secs as u64 * 1000) / poll_ms;
for _ in 0..iterations {
    if done_clone.load(Ordering::Relaxed) { return; }
    if stop_for_thread.load(Ordering::Relaxed) {
        cancelled_clone.store(true, Ordering::Relaxed);
        let pgid = Pid::from_raw(pid);
        let _ = signal::killpg(pgid, signal::Signal::SIGTERM);
        std::thread::sleep(Duration::from_millis(500));
        let _ = signal::killpg(pgid, signal::Signal::SIGKILL);
        return;
    }
    std::thread::sleep(Duration::from_millis(poll_ms));
}
```

The existing test `test_bash_cancels_on_stop_flag` already exercises this path —
it sleeps 100ms then sets the stop flag and expects return within ~1s. The test
passes either way; it does not need to be updated, but tighten the "within ~1s"
assertion comment to "within ~200ms" to lock in the improvement.

**Verify:**

- `cargo test --workspace` — all tests pass (including the existing cancellation test).
- Manual: send a message that triggers `sleep 30`; press Esc; observe `⏸ Cancelling…`
  immediately and `✋ Cancelled` within 100ms (visibly faster than before).

**Notes:**
- Modified: `agent-core/src/tools/bash.rs` — reduced poll interval from 100ms to 5ms,
  iteration count computed from `(timeout_secs * 1000) / poll_ms` instead of `timeout_secs * 10`.
- Verified: `cargo test --workspace` → 132 passed, `cargo clippy --workspace` → no new warnings.

---

## Step 3 — Proper input box with history and multiline

- [x] Replace `read_line_raw` with `InputLine` struct
- [x] Cursor movement: Left/Right/Home/End/Ctrl+A/E
- [x] Backspace: no-op when cursor is at column 0
- [x] Kill words/line: Ctrl+W, Ctrl+U, Ctrl+K
- [x] History: Up/Down cycles through past submissions
- [x] Alt+Enter inserts a literal newline into the buffer
- [x] Proper redraw for multi-line buffers

**Goal:** Replace the current `read_line_raw` function (which has no cursor
movement, silently misfires on backspace-past-start, and no history) with a
well-behaved `InputLine` that feels like a shell prompt.

**Crate decision — no new dependency:**  
All mainstream readline-replacement crates (`rustyline`, `reedline`, `liner`)
manage their own terminal raw-mode lifecycle via `termios`. They conflict with
the existing termion raw-mode that owns the session between `run_interactive`'s
`into_raw_mode()` call and program exit. Rolling `InputLine` directly on top of
termion (already a dep) is ~200 lines and avoids the conflict entirely.

**Spec details:**

### Data model

File: `agent-cli/src/interactive.rs` (add before `read_line_raw`; that function
is then replaced at all three call sites).

```rust
/// Single-line (or optionally multi-line) interactive input with history.
struct InputLine {
    buf: Vec<char>,           // full buffer as Unicode chars
    cursor: usize,            // insertion point: 0..=buf.len() (char index)
    history: Vec<String>,     // submitted lines, oldest first
    history_idx: Option<usize>, // None = editing live draft
    draft: String,            // saved live draft while browsing history
}

enum LineEvent {
    Continue,         // keep reading
    Submit(String),   // user pressed Enter; the string may contain '\n' chars
    Quit,             // Ctrl-C
}
```

### Key bindings

`fn handle_key(&mut self, key: Key, out: &mut impl Write) -> LineEvent`

| Key | Action |
|-----|--------|
| `Char('\r')` / `Char('\n')` | collect buffer as String, push to history if non-empty and not duplicate of last entry, return `Submit` |
| `Alt('\n')` | insert `'\n'` at cursor, advance cursor, redraw |
| `Backspace` | if cursor > 0: remove char at cursor-1, cursor -= 1, redraw; else no-op |
| `Delete` / `Ctrl('d')` | if cursor < buf.len(): remove char at cursor, redraw; else no-op |
| `Left` / `Ctrl('b')` | cursor = cursor.saturating_sub(1), redraw |
| `Right` / `Ctrl('f')` | cursor = min(cursor+1, buf.len()), redraw |
| `Home` / `Ctrl('a')` | cursor = 0, redraw |
| `End` / `Ctrl('e')` | cursor = buf.len(), redraw |
| `Up` / `Ctrl('p')` | history_prev(), redraw |
| `Down` / `Ctrl('n')` | history_next(), redraw |
| `Ctrl('u')` | remove buf[0..cursor], cursor = 0, redraw |
| `Ctrl('k')` | truncate buf at cursor, redraw |
| `Ctrl('w')` | kill word before cursor (skip spaces, then non-spaces), redraw |
| `Ctrl('c')` | return `Quit` |
| `Char(c)` (any other) | insert c at cursor, cursor += 1, redraw |
| everything else | `Continue` (ignore unknown escape sequences) |

History navigation:

```rust
fn history_prev(&mut self) {
    if self.history.is_empty() { return; }
    match self.history_idx {
        None => {
            // save current live draft
            self.draft = self.buf.iter().collect();
            self.history_idx = Some(self.history.len() - 1);
        }
        Some(0) => {}  // already at oldest
        Some(ref mut i) => { *i -= 1; }
    }
    if let Some(i) = self.history_idx {
        self.buf = self.history[i].chars().collect();
        self.cursor = self.buf.len();
    }
}

fn history_next(&mut self) {
    match self.history_idx {
        None => {}
        Some(i) if i + 1 >= self.history.len() => {
            // return to live draft
            self.history_idx = None;
            self.buf = self.draft.chars().collect();
            self.cursor = self.buf.len();
        }
        Some(ref mut i) => {
            *i += 1;
            let idx = *i;
            self.buf = self.history[idx].chars().collect();
            self.cursor = self.buf.len();
        }
    }
}
```

### Redraw

```rust
fn redraw(&self, out: &mut impl Write, prompt: &str, prev_lines: &mut usize) {
    // 1. Move to start of input area (up by prev_lines-1, then \r)
    if *prev_lines > 1 {
        write!(out, "{}", termion::cursor::Up((*prev_lines - 1) as u16)).ok();
    }
    write!(out, "\r{}", termion::clear::AfterCursor).ok();

    // 2. Write prompt + buffer, translating '\n' → "\r\n" for raw mode
    write!(out, "{}", prompt).ok();
    let content: String = self.buf.iter().collect();
    write!(out, "{}", content.replace('\n', "\r\n")).ok();

    // 3. Recount visual lines (prompt line + embedded newlines)
    *prev_lines = 1 + self.buf.iter().filter(|&&c| c == '\n').count();

    // 4. Reposition cursor.
    //    chars_after = buf.len() - cursor
    //    Move back by that many chars (accounting for '\n' as a line move).
    let chars_after = self.buf.len() - self.cursor;
    if chars_after > 0 {
        let suffix = &self.buf[self.cursor..];
        let newlines_in_suffix = suffix.iter().filter(|&&c| c == '\n').count();
        let cols_back = chars_after - newlines_in_suffix;  // chars on same/last line
        if newlines_in_suffix > 0 {
            write!(out, "{}", termion::cursor::Up(newlines_in_suffix as u16)).ok();
        }
        if cols_back > 0 {
            write!(out, "{}", termion::cursor::Left(cols_back as u16)).ok();
        }
    }

    out.flush().ok();
}
```

`termion::cursor::Up` and `termion::cursor::Left` are already available from the
`termion` dep. `termion::clear::AfterCursor` needs `use termion::clear` (not yet
imported — add it).

### Integration

Replace every call to `read_line_raw(keys, out)` in `run_interactive` with:

```rust
// Defined once at the start of run_interactive:
let mut input_line = InputLine::new();
let mut prev_lines = 1usize;

// At each prompt site:
write!(out, "\r\n> ").ok();
out.flush().ok();
prev_lines = 1;
let result = loop {
    match keys.next() {
        Some(Ok(k)) => match input_line.handle_key(k, out, "> ", &mut prev_lines) {
            LineEvent::Continue => {}
            ev => break ev,
        },
        Some(Err(_)) | None => break LineEvent::Quit,
    }
};
```

`InputLine::new()` creates an empty buffer with empty history. History persists
across prompts within one session (history lives on the `InputLine` instance in
`run_interactive`'s frame).

The `select_or_create_tree` and `create_tree_interactive` helpers take the
`InputLine` by `&mut` instead of `keys` directly, so they share history.

### Tests

Add to `#[cfg(test)] mod tests` in `interactive.rs`:

- `test_inputline_backspace_at_start_is_noop` — feed `Backspace` to an empty
  `InputLine`; `buf` stays empty, no panic.
- `test_inputline_cursor_movement` — insert "hello", move left twice, insert "X";
  result is "helXlo".
- `test_inputline_history_cycle` — submit "first", submit "second", navigate Up
  twice (lands on "first"), Down once (lands on "second"), Down once more (restores
  empty draft).
- `test_inputline_alt_enter_inserts_newline` — feed `Alt('\n')`; buf contains `'\n'`,
  Submit yields a string with `'\n'`.
- `test_inputline_ctrl_u_kills_to_start` — insert "hello", move left 2, Ctrl+U;
  buf is "lo", cursor is 0.
- `test_inputline_ctrl_k_kills_to_end` — insert "hello", move left 2, Ctrl+K;
  buf is "hel".

### Do not modify

- `process_message` — it uses `poll_key()` and reads raw stdin bytes directly
  during streaming; it is not affected by this change.
- Wire protocol, server, store.

**Verify:**

- `cargo test --workspace` — all existing tests pass, six new tests pass.
- `cargo clippy --workspace` — no new warnings.
- Manual: at the `> ` prompt, type "hello world", Left×5, Backspace×3 (yields
  "he world"), Home, type "X" (yields "Xhe world"), Enter — message sent correctly.
- Manual: submit two messages, press Up twice, Down once — correct history restored.
- Manual: type a message, Alt+Enter, type more, Enter — newlines preserved in
  submitted string.
- Manual: Backspace at empty prompt — no visual glitch, no panic.

**Notes:**
- Created / Modified: `agent-cli/src/interactive.rs` — added `InputLine` struct with
  `handle_key`, `history_prev`, `history_next`, `redraw` methods and `LineEvent` enum;
  replaced `read_line_raw` function and all call sites; added 6 new tests.
- Details: 7 key bindings implemented (arrows, home/end, backspace/delete, Ctrl+U/K/W,
  Alt+Enter, history Up/Down, Ctrl+C). Redraw uses `clear::AfterCursor`, cursor
  positioning via `termion::cursor::Up/Left`. Buffer cleared on submit.
- Verified: `cargo test --workspace` → 127 passed (6 new InputLine tests),
  `cargo clippy --workspace` → no new warnings.

---

## Step 4 — Output rendering cleanup

- [x] Suppress Entry events during live streaming (fix duplicate content)
- [x] Remove spurious Done at end of history replay
- [x] Smart tool-arg display (extract command/path, not raw JSON)
- [x] Remove redundant "Assistant:" header
- [x] Merge ToolStart + ToolResult into a single block
- [x] Lighter user label and session header

**Goal:** Fix two rendering bugs (duplicate content, spurious Done), then apply a
focused set of visual improvements so the output is clean and easy to scan.

### Bug 1 — Duplicate content during streaming

**Root cause:** `render_event` handles both streaming event variants
(`TextChunk`, `ToolStart`, `ToolResult`) *and* `Entry` variants. The server
emits Entry events alongside streaming events to notify the CLI that content has
been persisted. Because `process_message` passes all incoming events through
`render_event`, every piece of content renders twice: once from the streaming
event and once from the Entry event.

**Fix:** In `process_message`, skip `ServerEvent::Entry` events entirely:

```rust
TryEvent::Event(ev) => {
    if matches!(&ev, ServerEvent::Entry(_)) { continue; }
    let done = matches!(&ev, ServerEvent::Done { .. });
    render_event(out, &ev, &mut state);
    if done { return Ok(()); }
    continue;
}
```

`Entry` events are already used by `replay_entries` for history display; they
serve no additional purpose in the live streaming path.

### Bug 2 — Spurious Done at session entry

**Root cause:** `replay_entries` (`interactive.rs:149-158`) appends
`render_done(out, "complete")` when the last replayed entry is not a
`SessionEnd`. The intent was to mark the previous turn as finished, but it
produces a stray `✓ Done` at the end of history whenever you enter a tree,
with no corresponding turn having just completed.

**Fix:** Delete the trailing `render_done` call. The idle prompt appearing
immediately after is already sufficient to communicate that no turn is in
progress.

```rust
// Remove entirely:
if let Some(last) = entries.last() {
    if !matches!(last, Entry::SessionEnd { .. }) {
        if in_turn { … blue separator … }
        render_done(out, "complete");   // ← delete this
    }
}
```

Keep the blue separator if `in_turn` — that visual break between history and
the prompt is still useful.

### Style 1 — Smart tool-arg display

File: `interactive.rs`, `render_event`, `ServerEvent::ToolStart` arm.

Currently: `🛠  find: {"pattern":"*.py","type":"file"}` — raw JSON.

Replace `args_str` construction with a helper that extracts the most meaningful
single argument for known tool names, falling back to a truncated JSON string:

```rust
fn format_tool_args(tool: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() { Some(o) => o, None => return String::new() };
    let pick = match tool {
        "bash"             => obj.get("command"),
        "read" | "write"
        | "edit"           => obj.get("path"),
        "find"             => obj.get("pattern").or_else(|| obj.get("path")),
        "grep"             => obj.get("pattern"),
        "git"              => obj.get("command").or_else(|| obj.get("args")),
        _                  => None,
    };
    match pick.and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let raw = serde_json::to_string(input).unwrap_or_default();
            if raw.len() > 80 { format!("{}…", &raw[..80]) } else { raw }
        }
    }
}
```

Display: `  ⚙ bash  python fibonacci.py` (two spaces of indentation, `⚙` in
dim/default color, tool name bold, arg in default color).

### Style 2 — Remove "Assistant:" header

File: `interactive.rs`, `render_event`, `ServerEvent::TextChunk` arm.

The `  Assistant:\r\n` header printed on `!state.assistant_header_shown` is
redundant — text following tool results is obviously the model's response.
Remove the header write; keep `assistant_header_shown` only to decide whether
to emit a leading blank line before the first chunk of a new assistant turn (so
text doesn't run into the previous line):

```rust
ServerEvent::TextChunk { content } => {
    if !state.assistant_header_shown {
        state.assistant_header_shown = true;
        write!(out, "\r\n").ok();   // blank line before first chunk only
    }
    write!(out, "{}", normalize_for_raw(content)).ok();
    out.flush().ok();
}
```

### Style 3 — Merge ToolStart + ToolResult into one block

Currently `ToolStart` prints a line immediately when the tool is called, and
`ToolResult` prints a second block when it returns. This produces two visually
separate entries for every tool call.

Change:
- `ToolStart`: print nothing (or print a dim "  ⚙ bash  …" line with a trailing
  `…` to indicate in-progress — but only if this is useful during long calls;
  for simplicity, **suppress ToolStart entirely** since the result follows
  quickly and the Result contains everything needed).
- `ToolResult`: print a single combined block:

```
  ⚙ bash  python fibonacci.py  (exit 0)
  │ First 10 Fibonacci numbers: [0, 1, 1, 2, 3, 5, 8, 13, 21, 34]
```

Format: `  ⚙ {tool}  {args}  (exit {code})` — bold tool name, dim exit code
when 0, red when non-zero. Output lines follow with `  │ ` prefix as today.
If output is empty, omit the output block entirely.

`RenderState` gains `last_tool_start: Option<(String, serde_json::Value)>` so
`ToolResult` can recover the tool name and args for its single-line header
(since ToolResult carries tool name and exit but not the input args):

```rust
struct RenderState {
    assistant_header_shown: bool,
    last_tool_args: Option<(String, serde_json::Value)>,  // (tool_name, input)
}
```

`ToolStart` arm stores into `state.last_tool_args` and returns without writing
anything. `ToolResult` arm reads `state.last_tool_args.take()` to get the args.

### Style 4 — User label and session header

**User label:** Replace `{}●  {}User:{}  {}` with `{}▸{} {}`:

```rust
write!(out, "\r\n{}▸{} {}\r\n",
       color::Fg(color::Green), style::Reset, text).ok();
```

One character, no colon, no double-space. The green `▸` is enough to mark user
input visually distinct from assistant text.

**Session header:** Replace the two heavy blue separator lines with a single
compact header:

```
{bold}{tree_title}{reset}  {dim}·  {short_id}{reset}
```

No `──────` lines. The title is the dominant element; ID is dimmed to the right.
Keep a blank line above and below. Example:

```
untitled  ·  87eb722e

[history replay…]

▸ run the python script
```

Use `color::Fg(color::LightBlack)` for the dimmed `·  id` portion.

### Tests

- `test_render_tool_suppresses_start` — drive `render_event` with a
  `ToolStart{bash}` followed by `ToolResult{bash, exit:0, output:"hi"}`; the
  combined output should contain `⚙` exactly once and contain "hi".
- `test_render_no_assistant_header` — drive `render_event` with a `TextChunk`;
  output must contain the chunk text and must NOT contain "Assistant".
- `test_format_tool_args_bash` — `format_tool_args("bash", json!({"command":"ls","description":"d"}))` == `"ls"`.
- `test_format_tool_args_fallback` — unknown tool with no matching key falls
  back to a JSON string.

### Do not modify

- `replay_entries` entry rendering logic beyond the Done removal.
- Wire protocol, server, agent-core.
- `process_message`'s Esc / cancel path.
- The existing `render_done` function (status labels unchanged).

**Verify:**

- `cargo test --workspace` — all tests pass, four new tests pass.
- `cargo clippy --workspace` — no new warnings.
- Manual: start a session on an existing tree; confirm history replays without
  a trailing `✓ Done`.
- Manual: send a message that calls bash; confirm each tool call appears exactly
  once, with clean arg display and no "Assistant:" label.
- Manual: send a message that returns a non-zero exit code; confirm it renders
  in red.

**Notes:**
- Modified: `agent-cli/src/interactive.rs` — all 6 items implemented:
  1. Suppressed Entry events in `process_message` via `if matches!(&ev, ServerEvent::Entry(_)) { continue; }`
  2. Removed `render_done(out, "complete")` from end of `replay_entries`
  3. Added `format_tool_args` helper extracting `command`/`file_path`/`pattern` per tool name, falling back to truncated JSON
  4. Removed `Assistant:` header from `TextChunk` arm
  5. Added `last_tool_args` to `RenderState`; `ToolStart` stores args and suppresses output; `ToolResult` renders combined `⚙ {tool}  {args}  (exit {code})` line with output
  6. Replaced user label `●  User:` with green `▸`; replaced session header `───` separators with compact `{title}  ·  {short_id}`
- Added 4 new tests: `test_render_tool_suppresses_start`, `test_render_no_assistant_header`, `test_format_tool_args_bash`, `test_format_tool_args_fallback`
- Verified: `cargo test --workspace` → 136 passed, `cargo clippy --workspace` → no new warnings.

---

## Step 5 — Surface worker crash output in the CLI

- [x] Accumulate worker stderr in a shared ring buffer
- [x] Include last N stderr lines in the error event on unexpected exit

**Goal:** When the worker crashes or fails, the CLI shows the actual error output
(panic message, stderr lines) instead of the generic "worker exited unexpectedly".

**Root cause:** `spawn_stderr_demux` (`lifecycle.rs:350`) reads worker stderr and
forwards each line only to the server log (`log::info!`). `spawn_stdout_proxy`
detects the unexpected exit and sends `ServerEvent::Error { message: "worker
exited unexpectedly" }` — with no access to the stderr content.

**Spec details:**

File: `agent-server/src/lifecycle.rs`.

### 1. Shared stderr ring buffer

Add a type alias at the top of the file:

```rust
type StderrBuf = Arc<Mutex<VecDeque<String>>>;
```

Create it in `spawn_worker`, before spawning the threads, and pass it to both
thread-spawning functions:

```rust
let stderr_buf: StderrBuf = Arc::new(Mutex::new(VecDeque::with_capacity(20)));
spawn_stdout_proxy(tree_id.to_string(), stdout, entry.clone(), store, cfg,
                   stderr_buf.clone());
spawn_stderr_demux(tree_id.to_string(), stderr, stderr_buf);
```

### 2. `spawn_stderr_demux` — fill the buffer

Change signature:

```rust
fn spawn_stderr_demux(tree_id: String, stderr: ChildStderr, buf: StderrBuf)
```

In the read loop, push each line into `buf` (cap at 20; drop the oldest when
full) and continue logging to `log::info!` as today:

```rust
Ok(_) => {
    let line = buf.trim_end().to_string();
    log::info!("[worker {}] {}", short, line);
    let mut g = buf.lock().unwrap();
    if g.len() >= 20 { g.pop_front(); }
    g.push_back(line);
}
```

### 3. `spawn_stdout_proxy` — include stderr in crash event

Change signature:

```rust
fn spawn_stdout_proxy(
    tree_id: String,
    stdout: ChildStdout,
    entry: Arc<Mutex<WorkerEntry>>,
    store: Arc<Store>,
    cfg: Arc<Config>,
    stderr_buf: StderrBuf,
)
```

In the `if !exit_ok` block, build the error message from the buffer:

```rust
if !exit_ok {
    log::warn!("[proxy {}] worker exited with error", tree_id);
    let detail = {
        let g = stderr_buf.lock().unwrap();
        if g.is_empty() {
            String::new()
        } else {
            format!("\n{}", g.iter().cloned().collect::<Vec<_>>().join("\n"))
        }
    };
    let err_event = ServerEvent::Error {
        message: format!("worker exited unexpectedly{}", detail),
        fatal: true,
    };
    …
}
```

### No CLI changes needed

`ServerEvent::Error { message, fatal: true }` already renders in the CLI as
`✖ Fatal: {message}`. Multi-line messages render correctly because
`print_error` writes the whole string and raw-mode newlines are handled by the
existing `\r\n` in the format string — except that embedded `\n` need to
become `\r\n`. Add a `normalize_for_raw` call in `print_error`:

```rust
fn print_error(out: &mut impl Write, text: &str) {
    let text = normalize_for_raw(text);
    write!(out, "{}{}✖ {}{}\r\n", …).ok();
}
```

`normalize_for_raw` already exists in `interactive.rs`.

**Verify:**

- `cargo test --workspace` — all tests pass.
- Manual: cause a worker crash (e.g. break the config so the worker panics on
  start); confirm the CLI shows the Rust panic message rather than only "worker
  exited unexpectedly".

**Notes:** _(filled on completion)_
- Modified: `agent-server/src/lifecycle.rs` — added `StderrBuf` type alias, created ring buffer in `spawn_worker`, passed to both `spawn_stdout_proxy` and `spawn_stderr_demux`; `spawn_stderr_demux` fills buffer (cap 20); `spawn_stdout_proxy` reads buffer on crash to include stderr in error message.
- Modified: `agent-cli/src/interactive.rs` — `print_error` now calls `normalize_for_raw` so multi-line error messages render correctly in raw mode.
- Verified: `cargo test --workspace` → all pass, `cargo clippy --workspace` → no new warnings.

---

## Step 6 — Thinking stream visibility

- [x] Add `ThinkingChunk` to `ServerEvent`
- [x] Add `reasoning_content` to `Delta`
- [x] Split `<think>` / `</think>` tags out of content deltas in the agent loop
- [x] Emit `ThinkingChunk` events; keep thinking out of persisted `response_text`
- [x] Render thinking in dim dark-gray, streaming in real time
- [x] Reset color cleanly when the first regular `TextChunk` follows

**Goal:** When the active model emits a reasoning / thinking trace (either via a
`reasoning_content` delta field or embedded `<think>…</think>` tags), stream it
to the terminal in visibly dimmer text so the user can watch the model think in
real time. Thinking content is never stored — it is ephemeral display only.

### Background

Two conventions exist in OpenAI-compatible providers for surfacing model
reasoning:

1. **Explicit field:** `delta.reasoning_content` (e.g. some DeepSeek / Qwen
   API variants). Non-null when the model has a separate reasoning budget.
2. **Inline tags:** `<think>…</think>` embedded in `delta.content` (DeepSeek
   R1 open-weight, Qwen3 thinking mode). The tags may straddle chunk
   boundaries, so they must be parsed stateously.

Both paths produce `ServerEvent::ThinkingChunk` events; the CLI renders them
identically.

### Spec details

#### 1. New event variant — `agent-core/src/types.rs`

Add to `ServerEvent`:

```rust
/// Reasoning/thinking token from models that expose extended thinking.
/// Never stored. Rendered in dimmed color during streaming.
#[serde(rename = "thinking_chunk")]
ThinkingChunk { content: String },
```

Add `reasoning_content` to `Delta` (same file):

```rust
pub struct Delta {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<DeltaToolCall>,
    // Explicit reasoning field (some providers). Absent → None.
    #[serde(default)]
    pub reasoning_content: Option<String>,
}
```

#### 2. Tag splitter — `agent-core/src/agent.rs`

Add before the agent loop:

```rust
enum ThinkingSegment {
    Thinking(String),
    Text(String),
}

/// Split a content delta into alternating thinking/text segments.
///
/// `in_thinking` tracks whether the parser is currently inside a `<think>` block
/// across chunk boundaries. Both `<think>` and `</think>` may land in the middle
/// of a chunk or straddle two chunks; the caller preserves `in_thinking` between
/// calls to handle the cross-boundary case.
fn split_thinking_chunks(text: &str, in_thinking: &mut bool) -> Vec<ThinkingSegment> {
    let mut result = Vec::new();
    let mut rest = text;
    loop {
        if *in_thinking {
            match rest.find("</think>") {
                Some(pos) => {
                    if pos > 0 {
                        result.push(ThinkingSegment::Thinking(rest[..pos].to_string()));
                    }
                    *in_thinking = false;
                    rest = &rest[pos + "</think>".len()..];
                }
                None => {
                    if !rest.is_empty() {
                        result.push(ThinkingSegment::Thinking(rest.to_string()));
                    }
                    break;
                }
            }
        } else {
            match rest.find("<think>") {
                Some(pos) => {
                    if pos > 0 {
                        result.push(ThinkingSegment::Text(rest[..pos].to_string()));
                    }
                    *in_thinking = true;
                    rest = &rest[pos + "<think>".len()..];
                }
                None => {
                    if !rest.is_empty() {
                        result.push(ThinkingSegment::Text(rest.to_string()));
                    }
                    break;
                }
            }
        }
    }
    result
}
```

#### 3. Agent stream loop — `agent-core/src/agent.rs`

In the `'turn` loop, add `let mut in_thinking = false;` next to the existing
`let mut response_text = String::new();`.

Replace the existing content-delta block:

```rust
// Was: if let Some(delta) = &choice.delta.content { … }
// Now: handle reasoning_content first, then split content via tag parser.

// Explicit reasoning field (some providers)
if let Some(rc) = &choice.delta.reasoning_content {
    if !rc.is_empty() {
        let _ = event_tx.send(ServerEvent::ThinkingChunk { content: rc.clone() });
    }
}

// Content delta — split on <think> / </think> tags
if let Some(delta) = &choice.delta.content {
    if !delta.is_empty() {
        for segment in split_thinking_chunks(delta, &mut in_thinking) {
            match segment {
                ThinkingSegment::Thinking(t) => {
                    let _ = event_tx.send(ServerEvent::ThinkingChunk { content: t });
                }
                ThinkingSegment::Text(t) if !t.is_empty() => {
                    response_text.push_str(&t);
                    let _ = event_tx.send(ServerEvent::TextChunk { content: t });
                }
                _ => {}
            }
        }
    }
}
```

`in_thinking` resets to `false` at the top of each `'turn` iteration (declaration
is inside the loop). Thinking content is excluded from `response_text`, so it is
never persisted.

#### 4. CLI render state — `agent-cli/src/interactive.rs`

Add field to `RenderState`:

```rust
#[derive(Default)]
struct RenderState {
    _rendered: HashSet<String>,
    assistant_header_shown: bool,
    last_tool_args: Option<(String, serde_json::Value)>,
    in_thinking: bool,   // true while a thinking block is streaming
}
```

#### 5. Thinking render arm — `agent-cli/src/interactive.rs`

Add to `render_event` (before or after `TextChunk`):

```rust
ServerEvent::ThinkingChunk { content } => {
    if !state.in_thinking {
        state.in_thinking = true;
        // \x1b[2m = SGR "faint" — noticeably dimmer than LightBlack alone.
        write!(out, "\r\n{}\x1b[2m", color::Fg(color::LightBlack)).ok();
    }
    write!(out, "{}", normalize_for_raw(content)).ok();
    out.flush().ok();
}
```

Update the `TextChunk` arm to close the thinking block when it ends:

```rust
ServerEvent::TextChunk { content } => {
    if state.in_thinking {
        // Thinking ended — reset terminal color and add a blank separator line.
        state.in_thinking = false;
        write!(out, "{}\r\n", style::Reset).ok();
    }
    if !state.assistant_header_shown {
        state.assistant_header_shown = true;
        write!(out, "\r\n").ok();
    }
    write!(out, "{}", normalize_for_raw(content)).ok();
    out.flush().ok();
}
```

The separator line between the thinking block and the main response keeps them
visually distinct without a hard divider.

### Tests

Add to `#[cfg(test)] mod tests` in `agent.rs`:

- `test_split_no_tags` — `split_thinking_chunks("hello world", &mut false)` →
  `[Text("hello world")]`, `in_thinking` still false.
- `test_split_full_block` — input `"<think>reason</think>answer"` →
  `[Thinking("reason"), Text("answer")]`, `in_thinking` false after.
- `test_split_open_only` — `in_thinking=false`, input `"<think>partial"` →
  `[Thinking("partial")]`, `in_thinking` true after.
- `test_split_close_only` — `in_thinking=true`, input `"end</think>rest"` →
  `[Thinking("end"), Text("rest")]`, `in_thinking` false after.
- `test_split_empty_think_block` — `"<think></think>after"` → `[Text("after")]`.

Add to `#[cfg(test)] mod tests` in `interactive.rs`:

- `test_render_thinking_chunk_faint` — `render_event` with a `ThinkingChunk`;
  output contains `\x1b[2m` (faint) and the chunk text.
- `test_render_thinking_resets_on_text` — drive `render_event` with
  `ThinkingChunk { "think" }` then `TextChunk { "answer" }`;
  output contains `style::Reset` (as string) before "answer" and contains
  both "think" and "answer".

### Do not modify

- The `<think>` / `</think>` tag text is stripped from the event content before
  sending — never written to the terminal or stored.
- `response_text` (and thus persisted messages) contains only non-thinking text.
- Existing `TextChunk` rendering path is unchanged except for the `in_thinking`
  reset guard added at its top.
- Wire protocol for existing events, store format, cancellation logic.

### Verify

- `cargo test --workspace` — all existing tests pass, seven new tests pass.
- `cargo clippy --workspace` — no new warnings.
- Manual (model with thinking, e.g. Qwen3 thinking mode or DeepSeek R1):
  1. Send a message that triggers reasoning.
  2. Observe the thinking trace streaming in dim dark-gray before the main
     response.
  3. Observe the color resets cleanly when the main response begins.
  4. Confirm the thinking text does NOT appear in `/entries` history.
- Manual (model without thinking, e.g. plain Qwen2.5):
  - No regression: output identical to before.

**Notes:**
- Modified: `agent-core/src/types.rs` — added `ThinkingChunk` variant to `ServerEvent`, added `reasoning_content` field to `Delta`.
- Modified: `agent-core/src/agent.rs` — added `ThinkingSegment` enum, `split_thinking_chunks` function (splits on ` thinking` / ` response` tags across chunk boundaries); modified stream loop to handle `reasoning_content` and tag-split content, emit `ThinkingChunk` events, exclude thinking from `response_text`.
- Modified: `agent-cli/src/interactive.rs` — added `in_thinking` field to `RenderState`; `render_event` now has `ThinkingChunk` arm (renders in dim `\x1b[2m` light-gray), `TextChunk` arm resets style when thinking block ends; added 7 new tests (5 for `split_thinking_chunks`, 2 for thinking rendering).
- Deviation: None — matches spec exactly.
- Verified: `cargo test --workspace` → 144 passed, `cargo clippy --workspace` → no new warnings.

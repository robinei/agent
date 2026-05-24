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
- **Queued input while the agent is working.** With the owned-region model the
  input box is always live; buffer submissions typed during streaming and flush
  them once the current turn ends. (Cancellation covers the immediate-stop case;
  this is the friendlier "I have a follow-up" case.)
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

```

---

## Pending Steps

### Migrate `interactive.rs` to `Terminal` + `MarkdownEmitter`

- [ ] Step 1: `terminal.rs` — add `timeout: Duration` parameter to `poll`; update `terminal_demo.rs` call site to pass `Duration::ZERO`
- [ ] Step 2: `interactive.rs` — delete removed infrastructure (see spec below)
- [ ] Step 3: `interactive.rs` — add `RenderState` and `render_event` free function
- [ ] Step 4: `interactive.rs` — add `wait_for_wakeup` (`#[cfg(unix)]` + `#[cfg(not(unix))]`)
- [ ] Step 5: `interactive.rs` — rewrite `process_message`
- [ ] Step 6: `interactive.rs` — rewrite `select_or_create_tree` / `create_tree_interactive`
- [ ] Step 7: `interactive.rs` — rewrite `run_interactive`
- [ ] Step 8: `interactive.rs` helper fns — convert `print_warning` / `print_error` / `print_help` / `print_tree_meta` to use `Terminal`
- [ ] Step 9: `agent-cli/Cargo.toml` — remove `termion` dependency

**Goal:** `termion` goes away completely. `TerminalRenderer`, `InputLine`, `LineEvent` go away with it. All terminal mechanism lives in `terminal.rs`; `interactive.rs` keeps only app logic.

**Spec:**

_Step 1 — `terminal.rs`: `poll` signature change_

```rust
// before
pub fn poll(&mut self) -> io::Result<Option<TermEvent>>
// after
pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<TermEvent>>
```
Inside: replace `event::poll(Duration::ZERO)` with `event::poll(timeout)`.
`terminal_demo.rs`: `term.poll(Duration::ZERO)?`

_Step 2 — Delete from `interactive.rs`:_

- All `use termion::…` imports
- `use nix::poll::…` (move under `#[cfg(unix)]` in step 4)
- `const SPINNER_FRAMES` and `const SPINNER_INTERVAL_MS` (duplicates of `terminal.rs`)
- `struct TerminalRenderer<'a>` + full `impl` block
- `fn normalize_for_raw`, `fn count_trailing_crlf`, `fn poll_key`
- `struct InputLine` + full `impl` block
- `enum LineEvent`
- Tests: `test_normalize_for_raw_*` and `test_inputline_*`
- Keep: `format_tool_args`, `test_format_tool_args_*`, `test_render_done_*`

Add imports:
```rust
use crate::terminal::{Span, TermEvent, Terminal};
use crate::markdown::MarkdownEmitter;
use crossterm::style::{Color, ContentStyle};
#[cfg(unix)] use std::os::unix::io::RawFd;
```

_Step 3 — `RenderState` + `render_event`:_

```rust
struct RenderState {
    trailing_newlines: u8,
    in_thinking: bool,
    assistant_header_shown: bool,
    last_tool_args: Option<(String, serde_json::Value)>,
}
```

`render_event` is a free function:
```rust
fn render_event(
    event: &ServerEvent,
    state: &mut RenderState,
    md: &mut MarkdownEmitter,
    term: &mut Terminal,
) -> io::Result<()>
```

Port of `TerminalRenderer::render_event`:
- `write_text(content)` → `md.push(content, term)?`
- `write_thinking(content)` → `term.append` with dim/grey styled spans
- `blank_line_sep()` → emit `\r\n` spans if `state.trailing_newlines < 2`
- `hide_spinner()` / `show_spinner()` → `term.set_spinner_active(false/true)?`
- `color::Fg(color::X)` → `ContentStyle { foreground_color: Some(Color::X), ..Default::default() }`
- `style::Bold` → `ContentStyle { attributes: Attributes::from(Attribute::Bold), ..Default::default() }`
- `write!(self.out, …)` → `term.append(&[Span::plain(…)])?; term.flush_append()?`
- On `Done`: call `md.flush(term)?` before returning
- `col` tracking removed entirely

Make `SPINNER_INTERVAL` in `terminal.rs` `pub(crate)` so `render_event` / `process_message` can import it.

_Step 4 — `wait_for_wakeup`:_

```rust
#[cfg(unix)]
fn wait_for_wakeup(ws_fds: &[RawFd], term: &mut Terminal, timeout: Duration) -> io::Result<Option<TermEvent>> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::unix::io::BorrowedFd;
    let stdin_fd = { use std::os::unix::io::AsRawFd; std::io::stdin().as_raw_fd() };
    let pt = PollTimeout::try_from(timeout.as_millis().min(u16::MAX as u128) as u16)
        .unwrap_or(PollTimeout::ZERO);
    let mut fds: Vec<PollFd> = ws_fds.iter()
        .map(|&fd| unsafe { PollFd::new(BorrowedFd::borrow_raw(fd), PollFlags::POLLIN) })
        .chain(std::iter::once(unsafe {
            PollFd::new(BorrowedFd::borrow_raw(stdin_fd), PollFlags::POLLIN)
        }))
        .collect();
    let _ = poll(&mut fds, pt);
    if fds.last().and_then(|f| f.revents()).map_or(false, |r| r.contains(PollFlags::POLLIN)) {
        term.poll(Duration::ZERO)
    } else {
        Ok(None)
    }
}

#[cfg(not(unix))]
fn wait_for_wakeup(_ws_fds: &[RawFd], term: &mut Terminal, timeout: Duration) -> io::Result<Option<TermEvent>> {
    term.poll(timeout)
}
```

The `ws_fds` parameter is `&[RawFd]` to handle the case of multiple concurrent WS connections to the same tree.

_Step 5 — `process_message` rewrite:_

```rust
fn process_message(
    backend: &Backend,
    tree_id: &str,
    text: &str,
    term: &mut Terminal,
    md: &mut MarkdownEmitter,
    stop: &AtomicBool,
) -> io::Result<()>
```

- `ws_fds`: `let ws_fds: Vec<RawFd> = session.as_raw_fd().into_iter().collect();`
- `term.set_spinner_active(true)?` at start
- Drain WS loop: call `render_event`, on `Done`/`Fatal` call `term.set_spinner_active(false)?` and return
- Ctrl-C check: `stop.load` → append interrupted message
- Wait: `wait_for_wakeup(&ws_fds, term, remaining_until_next_spinner_tick)?`
  - On `TermEvent::Cancel`: append cancelling message, `session.send_stop()`, set `cancel_signalled`
- Spinner advances automatically inside `Terminal`'s `render_owned_impl`; no manual `spin_frame`/`tick_spinner`

_Step 6 — `select_or_create_tree` / `create_tree_interactive`:_

New signatures:
```rust
fn select_or_create_tree(term: &mut Terminal, backend: &Backend) -> Result<String, String>
fn create_tree_interactive(term: &mut Terminal, backend: &Backend) -> Result<String, String>
```

Input loop: `term.poll(Duration::from_millis(16))` matching on `Submit`/`Cancel`. Output via `term.append` + `flush_append`.

_Step 7 — `run_interactive` rewrite:_

```rust
pub fn run_interactive(backend: &Backend, initial_repo_path: Option<String>, stop: &AtomicBool) -> Result<(), String>
```

- `Terminal::new("> ")` replaces `into_raw_mode` + `keys` + `InputLine`
- `MarkdownEmitter::new()` for markdown rendering
- `history: Vec<String>` + `history_idx: Option<usize>` replace `InputLine::history`
- Main loop: `term.poll(Duration::from_millis(16))?` matching `Submit`/`Cancel`/`HistoryPrev`/`HistoryNext`/`Resize`
- On `HistoryPrev`/`HistoryNext`: update `history_idx`, call `term.set_input(&history[i])?`
- `process_message` called with `&mut term, &mut md`
- `term.teardown()` on exit

_Step 8 — helper functions:_

`print_warning`, `print_error`, `print_help`, `print_tree_meta` are rewritten to accept `&mut Terminal` and use `term.append` + `ContentStyle` instead of `write!` + termion escapes. `normalize_for_raw` is not needed (Terminal handles `\n` → `\r\n`).

_Step 9 — `Cargo.toml`:_

Remove `termion` from `agent-cli/Cargo.toml`. Confirm `cargo build` succeeds and no termion references remain (`grep -r termion agent-cli/`).

**Verify:**

```sh
# No termion references remain
grep -r termion agent-cli/src/

# Builds clean
cargo build -p agent-cli 2>&1 | grep -E "^error"

# Tests pass
cargo test -p agent-cli 2>&1 | tail -20

# Smoke test: demo still works
cargo run --bin terminal_demo
```

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

### OwnedRegion + InputBox + StatusBar

**Goal:** Replace the direct-terminal `InputLine::redraw` with a clean owned-region
abstraction. Components produce `Frame`s (lines + cursor); one place handles all
cursor arithmetic, grow/shrink, resize, and scroll-region management. Then add a
persistent status bar and make the input box always visible — even while the agent
is streaming output above it.

---

#### Key design

**Unified model — owned region is always active.**
There is no "streaming mode" vs "input mode" switch. The owned region (input box
+ status bar) is set up once at session start and stays up until exit.
`TerminalRenderer` always writes via `OwnedRegion::append_above`; the cursor
lives in the input line permanently.

**`OwnedRegion::render(out, frame)`** — redraws the owned block in place:
```
Up(last_row_count)   go to top of owned region
\r + AfterCursor     erase old contents
write frame.lines    separated by \r\n, no trailing newline
set scroll region    \x1b[1;{H - frame_height}r  (protects owned rows)
position cursor      Up(rows_from_bottom) + Right(col)
```
Grow and shrink are automatic: `AfterCursor` always clears the old extent;
the scroll region is updated to the new height after each render.
Resize: if `tw` changed since the last render, recompute `last_row_count` using
the new width before the `Up()` — same logic as the existing `last_term_cols` fix.

**`OwnedRegion::append_above(out, content)`** — what `TerminalRenderer` calls
instead of writing directly. Saves cursor, jumps to `TerminalRenderer`'s tracked
position at the bottom of the scroll region, writes, restores:
```
\x1b[?25l            hide cursor (prevent flicker during jump)
\x1b[s               save cursor (in owned region)
\x1b[{bottom};{col}H jump to TerminalRenderer's write-head position
write content        \r\n at the scroll-region boundary causes scroll-up,
                     owned rows are unaffected
\x1b[u               restore cursor to input line
\x1b[?25h            show cursor
```
`TerminalRenderer` already tracks `col`; this determines the column for the jump.
After each `append_above` call, `TerminalRenderer` updates its tracked position
based on what it wrote.

**Grow/shrink of owned region**: when input wraps to more lines or shrinks back,
`render` handles it via `AfterCursor` + updated scroll region. No special cases.

---

#### Phase 1 — `OwnedRegion` + `Frame` (`agent-cli/src/owned_region.rs`)

- [ ] Define `Frame { lines: Vec<String>, cursor: Option<(usize, usize)> }`
      where `cursor` is `(row, col)` within `lines` (0-indexed)
- [ ] Define `OwnedRegion { last_row_count: usize, last_term_cols: usize }`
- [ ] Implement `OwnedRegion::setup(out)`:
  - Print `initial_height` newlines to push content up and create owned-region
    space; set initial scroll region; set `last_row_count = 0` (first `render`
    call writes from scratch)
- [ ] Implement `OwnedRegion::render(out, frame)`:
  - Query `(tw, th)` from `termion::terminal_size()`
  - If `tw != last_term_cols`: recompute `last_row_count` for the new width
    before doing `Up()`
  - `Up(last_row_count)` if > 0, then `\r` + `clear::AfterCursor`
  - Write `frame.lines` separated by `\r\n` (no trailing newline on last line)
  - Update scroll region: `write!(out, "\x1b[1;{}r", th - frame.lines.len())`
  - Position cursor: `Up(bottom_row - cursor_row)` then `Right(cursor_col)`
  - Store `last_row_count = frame.lines.len()`, `last_term_cols = tw`
- [ ] Implement `OwnedRegion::append_above(out, content, write_col)`:
  - `write_col` is `TerminalRenderer`'s current column (where to resume writing)
  - Query `(_, th)` from `termion::terminal_size()`; `bottom = th - last_row_count`
  - Emit: hide cursor, save, `\x1b[{bottom};{write_col+1}H`, content, restore,
    show cursor
- [ ] Implement `OwnedRegion::teardown(out)`:
  - `Up(last_row_count)`, `\r` + `AfterCursor`, reset scroll region `\x1b[r`

#### Phase 2 — `InputLine` becomes pure layout

- [ ] Add `InputLine::layout(prompt: &str, tw: usize) -> (Vec<String>, (usize, usize))`:
  - Returns the terminal lines the input occupies and `(cursor_row, cursor_col)`
    within those lines; no terminal I/O
  - Move all wrapping/row-count arithmetic from `redraw` here; reuse `vrows`
    closure and pending-wrap cursor formula
- [ ] Remove `InputLine::redraw`; remove `last_visual_line`, `total_visual_lines`,
      `last_term_cols` from the struct (all owned by `OwnedRegion` now)
- [ ] Every call site becomes:
  `let (lines, cursor) = input.layout(prompt, tw);`
  `region.render(out, Frame { lines, cursor: Some(cursor) });`

#### Phase 3 — `StatusBar` component

- [ ] Define `StatusBar { model: String, thinking_effort: Option<String>, prompt_tokens: u64, context_window: u64 }`
- [ ] `StatusBar::render_line(tw: usize) -> String`:
  - Format: `  <model>  thinking:<effort>  ctx: <used>/<limit>k (<pct>%)`
  - Pad to `tw` with spaces; wrap in `\x1b[48;5;236m...\x1b[m`
  - Omit thinking field when `thinking_effort` is `None`
- [ ] Initialize from `agent_core::config::load_config()` inside `run_interactive`

#### Phase 4 — Frame assembly + wiring

- [ ] Add `fn build_frame(input: &InputLine, status: &StatusBar, prompt: &str, tw: usize) -> Frame`:
  - `input.layout(prompt, tw)` → lines + cursor
  - Push `status.render_line(tw)` as the final line
  - Return `Frame { lines, cursor: Some(cursor) }`
- [ ] Replace all `input.redraw(...)` call sites with `region.render(out, build_frame(...))`
- [ ] Resize: `render` already handles it; no extra wiring needed
- [ ] Handle `Entry::ModelSet` → update `status.model`, call `region.render`
- [ ] Handle `ServerEvent::ContextUsage` → update `status.prompt_tokens`, call
      `region.render`

#### Phase 5 — `TerminalRenderer` uses `append_above`

- [ ] Add `write_col: usize` tracking to `TerminalRenderer` (it already tracks
      `col`; this is the same value, just plumbed through to `append_above`)
- [ ] Replace every `write!(self.out, ...)` in `TerminalRenderer` with calls to
      `region.append_above(out, content, self.col)`, updating `self.col` after
      each call as it does now
- [ ] After `append_above`, call `region.render(out, build_frame(...))` to redraw
      the owned region — this keeps the status bar and input box current after
      every streaming event
- [ ] Handle `ServerEvent::ContextUsage` and `Entry::ModelSet` in the streaming
      event loop: update `status`, then `region.render`

#### Phase 6 — Config + worker plumbing (configurable context window)

- [ ] Add `context_window: u64` (default `128_000`) to `SessionConfig` in
      `agent-core/src/config.rs`; TOML key `session.context_window`
- [ ] Add `context_window: u64` to `WorkerConfig` in `agent-core/src/rpc.rs`
      (`#[serde(default)]`)
- [ ] Add `ServerEvent::ContextUsage { prompt_tokens: u64 }` to
      `agent-core/src/types.rs`
- [ ] Pass `config.session.context_window` into `WorkerConfig` in
      `agent-server/src/lifecycle.rs`
- [ ] In `agent-worker/src/lib.rs`: pass `context_window` into `SessionConfig`
- [ ] In `agent-worker/src/turn.rs`: replace hardcoded `128_000` with
      `session_cfg.context_window`; emit `ContextUsage { prompt_tokens: estimated as u64 }`
      right after `estimate_context_tokens()`

---

**Verify:**
- Input wraps past terminal width: no duplicate lines, cursor correct
- Resize mid-input: redraws correctly, no line above input disturbed
- Input shrinks (backspace across wrap): extra row cleared cleanly
- Status bar visible during input, streaming, and spinner
- Context % updates after each turn; model name updates on `Entry::ModelSet`
- `session.context_window = 32000` in config → bar shows `/ 32k`
- Exit cleanly: status bar and owned region erased, scroll region reset

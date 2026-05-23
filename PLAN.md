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

```

---

## Pending Steps

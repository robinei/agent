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

### Edit tool: show post-edit context window

- [ ] Replace `build_diff` summary with a line-numbered context window around each edit
- [ ] Merge overlapping/adjacent windows, separate disjoint ones with `...`
- [ ] Update tests to match new output format

**Goal:** After applying edits the tool currently returns a terse `--- changes
(N edits)` summary. This is not enough for the LLM to verify its own work,
causing it to read the file again. Instead, return the final file content in a
window of ±N lines around each changed region, formatted identically to the
`read` tool (`{:>6} | {}\n`), with `...` between disjoint windows.

**Spec:**

- Context window: 3 lines before and after each edit's affected line range in
  the *final* file (i.e. after all edits applied). Configurable as a
  `CONTEXT_LINES: usize = 3` constant in `edit.rs`.
- Locate each edit's position in the final file by finding `new_text` after
  applying (the same position logic already used in `apply_edit`). Record the
  start/end line numbers of the replaced region in the final file.
- Build a list of `(start_line, end_line)` windows (1-indexed, inclusive),
  expand each by `CONTEXT_LINES`, clamp to file bounds, then merge overlapping
  or adjacent windows.
- Render: for each window emit numbered lines; between disjoint windows emit a
  single `...\n` line. Prepend a one-line header: `N edit(s) applied to path`.
- `build_diff` is replaced entirely; `_original` parameter can be dropped.
- No change to the `Tool` trait, `ToolOutput` fields, or the apply logic.

**Verify:**
```
cargo test -p agent-worker edit
```
All existing edit tests pass; new tests cover:
- single edit → window without `...`
- two disjoint edits → two windows separated by `...`
- two nearby edits → merged into one window
- edit at file start/end → window clamped correctly

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
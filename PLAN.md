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
- Disk layout verified: `.jsonl` (header + entries) + `.meta.json` written correctly

---

## Step 6 — Edit + Bash + Search tools

- [x] ✅ Done

**Created:**
- `agent-core/src/tools/edit.rs` — text replacement tool with:
  - Exact match first, then fuzzy fallback (NFKC normalize, trim trailing whitespace,
    normalize smart quotes→ASCII, dashes→`-`, special spaces→` `)
  - BOM stripping and CRLF→LF normalization
  - Multiple disjoint edits in one call (applied in reverse order for offset stability)
  - Per-file locking via `with_file_lock()` using canonicalized paths
  - Overlapping edit detection and error reporting
  - `resolve_path()` safety checks against repo root
- `agent-core/src/tools/bash.rs` — shell execution tool with:
  - Process group creation via `nix::unistd::setpgid` + `CommandExt::process_group(0)`
  - Timeout with SIGTERM escalation to SIGKILL after 5s grace period
  - Concurrent stdout/stderr reading via separate OS pipes
  - 2000-line / 50 KB output truncation
  - Environment variable passthrough
- `agent-core/src/tools/search.rs` — session/tree search tools:
  - `SearchMessagesTool` — scans JSONL files via `serde_json::StreamDeserializer`,
    supports filtering by tree ID, regex matching on message text and role
  - `SearchFilesTool` — walks `~/.agent/trees/` + arbitrary paths for artifacts

**Modified:**
- `agent-core/src/tools/mod.rs` — added `BashTool`, `EditTool`, `SearchMessagesTool`,
  `SearchFilesTool` to `all_tools()` registry (10 tools total)

**Deviations from PLAN.md:**
- `EditTool` uses a custom fuzzy normalization pipeline instead of a dedicated
  diff crate — keeps dependency count minimal
- `BashTool` reads stderr into the same output buffer as stdout (both are returned
  to the LLM) rather than separate error reporting
- `SearchFilesTool` is a simple `walkdir`-based file finder under store paths,
  not a full index — keeps it cheap for occasional use

**Verification:** `cargo test --workspace` — 58 tests pass, 0 clippy warnings in new code

---

## Step 7 — Agent loop + context building

- [x] ✅ Done

**Created:**
- `agent-core/src/agent.rs` — full agent loop with:
  - `build_context()` — walks parent chain from leaf_id up, collecting Messages,
    stopping at SessionEnd boundaries with continuation brief injection,
    tracking GoalSet/ModelSet
  - `estimate_tokens()` / `estimate_context_tokens()` — heuristic token counting
  - `run_agent()` — main loop: wait for user message, build context, load context files,
    check caps, call provider.stream_chat(), parse SSE chunks, dispatch tools,
    persist entries, emit ServerEvents
  - Tool execution with `catch_unwind` panic protection
  - 3 consecutive tool failure escalation
  - Max tool calls per turn guard
- `agent-core/src/context_files.rs` — AGENTS.md/CLAUDE.md discovery:
  - Walks up from cwd to root collecting context files
  - Checks `~/.agent/AGENTS.md` for global instructions
  - `.agent/skills/*/SKILL.md` discovery
  - `format_context_section()` for system prompt injection
- `agent-core/src/hooks.rs` — Hook trait + registry:
  - `on_tool_call`, `on_before_llm_call`, `on_session_end`, `on_startup` lifecycle hooks
  - Static registry with `register_hook()`, `run_tool_call_hooks()`, etc.
  - Test: rm -rf blocking hook

**Modified:**
- `agent-core/src/lib.rs` — added `pub mod agent`, `pub mod context_files`, `pub mod hooks`
- `agent-core/src/types.rs` — added `PartialEq` derive on `MessageRole`
- `agent-server/src/lifecycle.rs` — full agent spawn implementation:
  - Spawns agent thread with `run_agent()` loop
  - Spawns bridge thread that forwards events to SSE broadcast subscribers + ring buffer
  - `spawn()` takes `Arc<Store>` and `&Config`
  - `get_handle()` for SSE streaming access
- `agent-server/src/routes.rs` — added message, stop, stream routes:
  - `POST /trees/{id}/message` — send message to agent
  - `POST /trees/{id}/stop` — stop agent
  - `GET /trees/{id}/stream` — SSE event stream with `SseReconnectStream`
- `agent-server/src/main.rs` — added startup hooks call

**Deviations from PLAN.md:**
- `Store` needed `Clone` derive for Arc sharing across threads
- `run_agent()` is in `agent-core` but the `store` field `base_dir` is public — needed for
  context file loading from the agent dir
- SSE streaming uses `rouille::ResponseBody::from_reader()` instead of `from_stream_body()`
  (rouille 3.6 API)
- Bridge thread pattern: agent writes events to an mpsc channel, bridge reads and
  broadcasts to SSE subscribers. This decouples the agent from SSE delivery latency.
- `build_context()` doesn't pass through `continuation_brief` from every SessionEnd on the
  path — only the first one encountered (the nearest session boundary), which is the most
  relevant one

**Verification:** `cargo test --workspace` — 75 tests pass (10 new tests: 7 agent context building + 2 hooks + 6 context files)

---

## Step 8 — SSE streaming + event broadcast

- [x] ✅ Done

**What was implemented (mostly as part of Step 7):**
- `SseReconnectStream` in `routes.rs` — `Read` impl that serves SSE events with
  reconnection support. Serves catch-up events from the ring buffer first, then
  live events from the mpsc broadcast channel.
- `handle_sse_stream()` — `GET /trees/{id}/stream` route that creates an
  `SseReconnectStream` with the tree's event buffer and broadcast channel.
- Bridge thread in `lifecycle.rs::spawn()` — reads events from the agent's
  `event_tx` mpsc channel, populates the ring buffer (`event_buffer`, 1000 cap)
  for Entry events, and broadcasts to all SSE subscribers via `event_broadcast`.

  (Vec<mpsc::Sender<ServerEvent>>). Prunes disconnected subscribers on each send.

**New in Step 8:**
- **Auto-spawn agents on message:** `handle_send_message` in `routes.rs` now
  calls `lifecycle::spawn()` when no active agent exists for the tree, then
  retries sending the message. This makes `POST /trees/{id}/message` work
  without requiring a prior explicit spawn.
- **Config passed to route handler:** `main.rs` now wraps `Config` in `Arc` and
  passes `&Config` to `routes::handle_request()`, which threads it to
  `handle_send_message` (needed by `lifecycle::spawn()`).

**Modified:**
- `agent-server/src/main.rs` — wrap config in `Arc`, pass to route handler
- `agent-server/src/routes.rs`:
  - Added `use agent_core::config::Config`
  - `handle_request()` now takes `&Config` parameter
  - `handle_send_message()` now takes `&Arc<Store>` and `&Config` parameters,
    verifies tree exists, auto-spawns agent if none active

**Deviations from PLAN.md:**
- The bridge thread event emission logic is inline in `spawn()` rather than in a
  separate `emit_event()` helper function. Equivalent in behavior.
- `handle_sse_stream` returns 404 (not 409) when no agent is active for a tree
  — the caller should send a message first to auto-spawn.

**Verification:** `cargo check --workspace` — no warnings (dead code warnings
resolved since `lifecycle::spawn()` is now called from routes).
`cargo test --workspace` — 75 tests pass.
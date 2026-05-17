# Implementation Notes

This file tracks progress through the [Implementation Plan](PLAN.md#implementation-plan).
Each step gets a section appended at the end of its session, marking it done
with decisions, deviations, bugs, and verification commands.

---

## Step 1 — Workspace skeleton and core types

- [x] ✅ Done

**Created:**
- `Cargo.toml` — workspace root with resolver = "2", 3 members
- `agent-core/Cargo.toml` — all deps from plan (serde, uuid, chrono, ureq, nix, etc.)
- `agent-core/src/lib.rs` — pub mod types, pub mod tools
- `agent-core/src/types.rs` — all types (1000+ lines): TreeMeta, Entry, Message, MessageContent,
  ToolCall, ChatStream, ServerEvent, ToolDefinition, ToolOutput, etc.
- `agent-core/src/tools/mod.rs` — stub (filled in Step 4)
- `agent-server/Cargo.toml` + `main.rs` — rouille + agent-core dep, hello stub
- `agent-cli/Cargo.toml` + `main.rs` — clap + termion + agent-core hello stub

**Deviations from PLAN.md:**
- `ToolDefinition` uses `String` fields instead of `'static str` — no lifetime issues in Vec<>
- `MessageContent::default()` is manual impl instead of `#[default]` (non-unit variant)
- `ChatStream::new()` takes `ureq::BodyReader<'static>` directly instead of `ureq::Response`
  because `ureq::Response` is a re-export of `http::Response<T>` in ureq 3 and its constructor
  path is private; the caller calls `response.into_reader()` and passes the reader
- `Entry::Label` given all the standard fields (id, parent_id, timestamp, label)

**Verification:** `cargo build --workspace` compiles with zero warnings from our code.

---

## Step 2 — Config + logging + store (JSONL I/O)

- [x] ✅ Done

**Created:**
- `agent-core/src/config.rs` — `Config` struct (5 subsections), `load_config()` with env var
  overrides, minimal TOML parser (no extra dep), `agent_dir()` helper
- `agent-core/src/logging.rs` — `FileLogger` implementing `log::Log`, thread-local `AGENT_TREE_ID`,
  `init_logging()` registering env_logger + file logger
- `agent-core/src/store.rs` — `Store` struct with base_dir, all JSONL I/O:
  - `create_tree_file()` / `append_entry()` / `read_all_entries()`
  - `load_tree_meta()` / `save_tree_meta()` (atomic write+rename)
  - `get_tree()` / `update_tree()` / `list_trees()` (backed by INDEX_CACHE)
  - `rebuild_index()` (scans `trees/*.meta.json`)
  - `update_header()` / `reset_header_tokens()`
- Wired all 3 modules into `lib.rs`

**Deviations from PLAN.md:**
- `Store` is a struct with `base_dir` field instead of free functions — enables test
  isolation with tempdirs instead of global env var
- Minimal TOML parser instead of `toml` crate — our config is simple enough
- `FileLogger` registered via `log::set_boxed_logger` (takes `Box<dyn Log>`), not `set_logger`
  (requires `&'static`)

**Bugs fixed:**
- `rebuild_index()` extension check: `{id}.meta.json` has extension `.json`, not `.meta`.
  Fixed to check `fname.ends_with(".meta.json")`

**Verification:** `cargo test --workspace` — 9 tests pass (config: 3, logging: 1, store: 5)

---

## Step 3 — Provider (LLM API client)

- [x] ✅ Done

**Created:**
- `agent-core/src/provider.rs`:
  - `Provider` struct (base_url, api_key, model)
  - `stream_chat()` — streaming POST to `/v1/chat/completions`, returns `ChatStream`
  - `chat()` — non-streaming chat returning `ChatResponse`
  - `generate_continuation_brief()` — summarization via separate LLM call
  - `serialize_message()` — converts `Message` → OpenAI API JSON format
  - `build_body()` — constructs request body with streaming options
- Wire `pub mod provider` in `lib.rs` in `lib.rs`
- Enabled `features = [\"json\"]` on ureq dependency

**Deviations from PLAN.md:**
- ureq 3 uses `.header()` not `.send_json()` returns errors as `ureq::Error::StatusCode` for non-2xx
- `generate_continuation_brief()` includes fallback text when LLM returns empty brief

**Bugs fixed:**
- ureq `json` feature not in defaults — had to add `features = [\"json\"]`

**Verification:** `cargo test --workspace` — 14 tests pass (5 new provider tests)

---

## Step 4 — Tool system (trait + initial tools)

- [x] ✅ Done

**Created:**
- `agent-core/src/tools/mod.rs` — `Tool` trait (`definition()` + `execute()`), `ToolResult` type alias,
  `all_tools()` registry function, `resolve_path()` helper (path safety guard), `truncate_output()` helper
- `agent-core/src/tools/read.rs` — `ReadTool`: reads files with 2000 line / 50 KB limit, offset/limit params,
  line numbers, filesystem safety checks
- `agent-core/src/tools/write.rs` — `WriteTool`: writes files, creates parent dirs, path escape detection
  by normalizing `..` components against canonicalized `cwd`
- `agent-core/src/tools/ls.rs` — `LsTool`: directory listing with permissions, size, type. 500 entry max.
- `agent-core/src/tools/grep.rs` — `GrepTool`: recursive regex file search, skips `.git`/`node_modules`/`target`,
  context lines support, binary detection via null byte check
- `agent-core/src/tools/find.rs` — `FindTool`: glob/substring filename search via `walkdir`, type filter
  (file/dir/both), skips common non-source dirs
- `agent-core/src/tools/git.rs` — `GitTool`: wraps `git` subprocess, subcommands: status/diff/log/show/
  add/commit/push/pull, structured output with branch info and ahead/behind

**Deviations from PLAN.md:**
- `Tool` trait uses `Box<dyn std::error::Error + Send + Sync>` for errors instead of a custom `ToolError`
- `resolve_path()` returns `Option<PathBuf>` instead of `Result` — simpler to chain
- `truncate_output()` is a module-level utility (not per-method) — shared by multiple tools
- `all_tools()` takes `&Path`; tools store a `PathBuf` copy of the cwd
- `WriteTool` path safety uses manual `..` resolution + canonicalized `cwd` comparison,
  since the target file (and its parents) may not exist yet
- `GitTool` returns raw stdout/stderr rather than fully structured JSON — simpler and more
  flexible for the LLM to interpret
- `GrepTool` binary detection checks for null bytes in first 8 KB — fast heuristic, not full MIME
- `FindTool` glob matching is a simplified custom implementation (no glob crate dependency)

**Bugs fixed:**
- `WriteTool` path check failed on nonexistent parent directories — replaced canonicalize-based
  check with manual `..` component resolution against canonical cwd
- `GitTool` `run_git()` returns `Result` but was destructured as tuple — fixed to propagate `?`
  properly
- Use-after-move on `content.len()` / `stdout.len()` in `ToolOutput` construction — fixed to
  compute length before move
- Redundant `pattern == "*" || pattern == "*"` in find.rs — simplified to single check

**Verification:** `cargo test --workspace` — 40 tests pass (26 new tests across 7 tool modules)
`cargo clippy --lib -p agent-core` — no warnings in tools code
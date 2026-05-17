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

**Verification:** `cargo test --workspace` — 8 tests pass (config: 2, logging: 1, store: 5)

---
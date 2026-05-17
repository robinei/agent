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
# Recommended Fixes

Issues identified during code review, ordered by severity.

---

## 🔴 Critical

### 1. [DONE] Broken brace nesting in `config.rs` — silently breaks `[lsp]` TOML parsing

**File:** `agent-core/src/config.rs` (lines 301–340)

The `[lsp]` configuration section and everything after it is accidentally nested inside the `[sandbox.defaults]` handler due to a stray closing brace on line 309. The result is that `[lsp]` settings in `config.toml` are **only** parsed if `[sandbox.defaults]` also has a non-empty `hide` array. Otherwise they're silently ignored.

**Fix:** Restore proper brace nesting. The `[lsp]` block (lines 312–338) should be at the same level as the `[sandbox]` and `[session]` blocks — inside `apply_toml`, not inside the `[sandbox.defaults]` sub-handler. The closing `}` on line 339 closes the function body, but the function was never closed before it. The structure should be:

```rust
fn apply_toml(cfg: &mut Config, table: &toml::Table) {
    // [server] ...
    // [provider] ...
    // [summary] ...
    // [session] ...
    // [logging] ...
    // [sandbox] ...
    //   [sandbox.defaults] ...
    // [lsp] ...              <-- same level as [sandbox]
}
```

---

## 🟠 High

### 2. [DONE] Global statics create shared mutable state across tests and instances

**Files:** `agent-core/src/store.rs`, `agent-server/src/lifecycle.rs`

`INDEX_CACHE`, `FILE_LOCKS`, and `ACTIVE_WORKERS` are `LazyLock<Mutex<HashMap>>` statics. This means:

- Unit tests that write to the store or spawn workers can interfere with each other
- Multiple `Store` instances share the same file-lock pool, which is correct for a single-process server but breaks when tests are parallelised
- The `#[ignore = "needs investigation - test runner hangs"]` on `test_broadcast_meta_update` is almost certainly caused by stale entries from other tests

**Fix:** Remove the `INDEX_CACHE` static and store the cache on `Store` itself (behind an `Arc<Mutex<...>>`). For `ACTIVE_WORKERS`, either make it a parameter threaded through the server, or (if it must be a global for signal-handler access) ensure every test cleans up in a `Drop` impl or `#[ctor]` destructor.

### 3. [DONE] `do_tls_io` silently swallows `read_tls` errors

**File:** `agent-server/src/llm_handler.rs` (line 147)

```rust
let n = conn.read_tls(tcp).unwrap_or(0);
```

A TLS fatal alert or socket error causes `read_tls` to return `Err`. Swallowing it with `unwrap_or(0)` means the handler can spin indefinitely on a dead connection, never progressing and never sending `LlmResponse::Error` to the worker.

**Fix:** Propagate the error:

```rust
let n = conn.read_tls(tcp).map_err(|e| {
    log::error!("[LlmHandler {}] TLS read_tls error: {}", ctx.tree_id, e);
    send_llm_error(ctx, self.req_id, &format!("TLS read error: {e}"));
})?;  // or return false on error
```

### 4. [DONE] `restore_edit` can be called twice on the same record, corrupting files

**File:** `agent-worker/src/tools/restore_edit.rs` (conceptual)

The `EditRecord` has no `reverted` flag. If the LLM calls `restore_edit` with `revert_patch` on an already-reverted edit, the pre-snapshot is applied a second time, overwriting the current (correct) file content with the old state.

**Fix:** Add a `reverted: bool` field (or `applied_count: u32`) to `EditRecord`. Set it to `true` after a revert. `restore_edit` should refuse to revert an already-reverted record (or refuse to apply a patch that's already been applied). For `revert_patch`, also check that the patched text still matches before reversing.

### 5. REMOVED!

### 6. [DONE] Chunked-encoding terminal chunk handled as success, not EOF

**File:** `agent-server/src/llm_handler.rs` (lines 218–219)

When the HTTP chunked decoder reads the terminal chunk (size `0\r\n` in `ChunkDecode::Size`), `feed_bytes` returns `true`. The caller (`LlmState::Streaming`) then tries to read more data. The connection never sends more data, so the poll loop spins until the 30-second poll timeout, then on the next iteration the socket read returns `Ok(0)`, which sends `Llm::Done`.

**Fix:** When `ChunkDecode::Size` sees a zero-size chunk, signal EOF immediately:

```rust
if size == 0 {
    // Terminal chunk — flush any buffered SSE line and signal done
    if !self.line_buf.is_empty() {
        let line = std::mem::take(&mut self.line_buf);
        self.handle_sse_line(ctx, &line);
    }
    send_llm_done(ctx, self.req_id);
    return false;  // remove handler
}
```

---

## 🟡 Medium

### 7. [DONE] `StderrHandler` reads only one line per `on_ready` call

**File:** `agent-server/src/handlers.rs` (lines 126–141)

The `StdoutHandler` loops until `WouldBlock`, draining all available lines. The `StderrHandler` reads exactly one line. If the worker logs multiple lines (e.g. during startup), they're consumed one per poll cycle, needlessly slowing down the event loop.

**Fix:** Add an inner loop analogous to `StdoutHandler`:

```rust
fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool {
    loop {
        self.line_buf.clear();
        match self.reader.read_line(&mut self.line_buf) {
            Ok(0) => return false,
            Ok(_) => {}
            Err(e) if e.kind() == WouldBlock => return true,
            Err(_) => return false,
        }
        // process self.line_buf...
    }
}
```

### 8. [DONE] `header_contains` doesn't handle `Transfer-Encoding + Content-Length` conflicts

**File:** `agent-server/src/http.rs` (lines 93–107)

The server reads `Content-Length` to determine body size but never checks for `Transfer-Encoding: chunked`. An attacker sending both headers could exploit request smuggling.

**Fix:** After parsing headers, check for `Transfer-Encoding`. If present, use chunked framing instead of `Content-Length`. If both are present and disagree, reject with 400.

### 9. [DONE] Race between `WorkerMsg::Stop` and in-flight `LlmResponse::Chunk`

**File:** `agent-server/src/handlers.rs` (lines 185–188)

When `NotifyHandler` receives `WorkerMsg::Stop`, it sends `PipeIn::Cmd(Stop)` to the worker's stdin. But the worker's event loop (in `agent-worker/src/lib.rs`) reads all available messages in `parse_pipe_messages` and dispatches them in order. If a `LlmResponse::Chunk` or `LlmResponse::Done` was already in the stdin buffer (written by `LlmHandler` but not yet consumed by the worker), the stop command arrives after those messages. The worker processes the chunk, which calls `process_chunk`, potentially appending text to an already-cancelled response.

**Fix:** In the worker's `dispatch_pipe_in`, if a `Stop` arrives and the state is `Streaming`, skip any subsequent `Llm` messages in the same batch. Or, add a `stop_seen` flag that's checked before dispatching `Chunk`/`Done`/`Error`.

### 10. REMOVED!

---

## 🔵 Low / Style

### 11. [DONE] `unsafe` raw-fd borrows lack safety comments

**Files:** `agent-server/src/worker_loop.rs`, `agent-server/src/lifecycle.rs`, `agent-worker/src/lib.rs`

`BorrowedFd::borrow_raw` is used with `.unwrap()` after `PollFd::new`. While the invariants hold (the fd lives at least as long as the poll call), each call site should document *why* the borrow is safe:

```rust
// SAFETY: child_stdout is owned by StdoutHandler for the lifetime of the loop.
PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, flags)
```

### 12. [DONE] `unwrap()` on `Mutex::lock` can panic the whole server on poisoning

**Files:** `agent-core/src/store.rs`, `agent-server/src/lifecycle.rs`

`INDEX_CACHE.lock().unwrap()` and `ACTIVE_WORKERS.lock().unwrap()` will panic if another thread panicked while holding the lock (mutex poisoning). For a server process, this brings down all active trees.

**Fix:** Use `lock().map_err(|e| ...)` and propagate, or use `LockResult::into_inner()` to recover from poisoning, or at least catch_unwind around thread entry points to prevent poisoning in the first place.

### 13. [DONE] Unused variable `let _ = found_model;`

**File:** `agent-worker/src/agent.rs` (line 89)

The variable `found_model` is populated but deliberately discarded. This should either be prefixed with `_` to suppress the warning without a statement, or actually used (e.g., emit a model-set system message in the context).

**Fix:** Change `let mut found_model = None;` to `let mut _found_model = None;`.

# Plan (tokio + reqwest + axum — canonical)

Steps for ongoing work on the agent server. Architecture overview and
"how things fit together" live in `AGENTS.md`; troubleshooting recipes in
`DEBUG.md`. This file is only step specs (what to build next).

This is the **canonical** async-migration plan. It supersedes both `PLAN.md`
(hand-rolled-TLS tokio draft) and `PLAN2.md` (smol/surf draft). The decisive
reasons it wins over both:

- **`reqwest` deletes the hand-rolled HTTPS client entirely.** PLAN.md still
  hand-rolled TLS + SSE with `tokio-rustls` (~150 lines); the current code is
  ~515 lines (`llm_handler.rs`). `reqwest` with the `rustls-tls` backend
  reduces the streaming LLM call to ~30 lines, with pure-Rust TLS and
  connection reuse for free.
- **Pure Rust, no C dependency.** `reqwest` + `rustls-tls` needs no
  `libcurl`/`openssl` — unlike `surf`'s curl backend (the reason PLAN2's
  "avoid async-std" road kept dragging libcurl along).
- **`axum` makes the WebSocket upgrade trivial** (`WebSocketUpgrade`), which
  was the single fiddly part of every hand-rolled / smol option.
- **`current_thread` runtime keeps it single-threaded and light** — no
  work-stealing scheduler we don't need — while the block_on-vs-spawn split
  (see Concurrency model) gives us `!Send` freedom in the event loop with
  zero `LocalSet` ceremony.

---

## Async stack

| Concern              | Crate / API                                  | Notes                                                                |
| -------------------- | -------------------------------------------- | -------------------------------------------------------------------- |
| Runtime              | `tokio` `new_current_thread`                 | One runtime per binary; `block_on(run())`. No multi-thread scheduler.|
| HTTP server + router | `axum` (`ws` feature) on `hyper`             | 4 REST routes + 1 WS route. axum 0.8 path params are `/{id}`.        |
| WebSocket (server)   | `axum::extract::ws::WebSocketUpgrade`        | `ws.on_upgrade(...)`; replaces all hand-rolled handshake code.       |
| HTTP client (LLM)    | `reqwest` (`rustls-tls`,`json`,`stream`)     | streaming SSE via `bytes_stream()`; pure-Rust TLS, **no C deps**.    |
| HTTP client (CLI)    | `reqwest`                                     | tree CRUD JSON; replaces `ureq`.                                     |
| WebSocket (CLI)      | `tokio-tungstenite`                          | `connect_async`; replaces blocking `tungstenite`.                    |
| File I/O             | `tokio::fs`                                   | `tree_io` + `Store`; atomic writes via `tokio::fs::rename`.          |
| Channels (mpsc)      | `tokio::sync::mpsc`                           | event funnel + cmd pipes; replaces `std::sync::mpsc`/notify-pipe.    |
| Fan-out              | `tokio::sync::broadcast`                      | `ServerEvent` to WS clients; **Lagged → disconnect + resync**.       |
| Child processes      | `tokio::process::{Command, Child}`           | **kill via `start_kill()` (sync)** in GC; `.kill()` is async.        |
| Signals              | `tokio::signal` (`ctrl_c`, `unix::signal`)   | SIGINT + SIGTERM funneled into the event channel.                    |
| Timers               | `tokio::time::{sleep_until, interval}`        | LSP wait deadlines, CLI render tick.                                 |
| Stream adapters      | `tokio-util` (`io` feature)                   | `StreamReader` to turn `bytes_stream()` into `AsyncBufRead`.         |
| Terminal input (CLI) | `crossterm::event::EventStream`               | feature `event-stream`; runtime-agnostic, funneled into events.      |
| Async tool trait     | `async-trait` (agent-worker)                  | `dyn Tool` dispatch; bodies pick `tokio::process`/`tokio::fs`/`spawn_blocking`. |

`reqwest` is declared `default-features = false, features = ["rustls-tls",
"json", "stream"]` so neither `openssl` nor `native-tls` (and thus no C
toolchain dependency) enters the tree. CI asserts this (see Conventions).

---

## Concurrency model (read this before writing any loop)

The whole design rests on one fact: **`Runtime::block_on(fut)` does not
require `fut: Send`; only `tokio::spawn(fut)` does.** We exploit that:

- **The main event loop runs inside `block_on` and may hold `!Send` state**
  (`Rc<RefCell<…>>`, non-`Send` handles) freely. No `Send` bound, no
  `LocalSet`, no `spawn_local`.
- **Every concurrent I/O source is a small `tokio::spawn`ed forwarder task**
  whose body is trivially `Send + 'static` (it moves in one reader + a cloned
  `mpsc::Sender`, reads *complete* messages, and forwards a typed `Event`).
- **The main loop is then just** `while let Some(ev) = rx.recv().await { … }`.

This is the **event-funnel pattern**, and it does three things at once:
1. Sidesteps the `current_thread`-still-needs-`Send`-for-`spawn` wart — we
   never `spawn` `!Send` work, so we never need a `LocalSet`.
2. Is **cancellation-safe by construction**: each forwarder owns its reader and
   only emits complete items, so no future is ever dropped mid-read.
   (Do **not** put `AsyncBufReadExt::read_line` directly in a `select!` — it is
   not cancel-safe; a dropped partial read corrupts the command stream.)
3. Collapses N event sources into one `match`, which is far easier to audit
   than a multi-branch `select!`.

The **server** is the exception and that's fine: axum/hyper handlers are
naturally `Send` (tree CRUD over `tokio::fs` + serde, WS over `Arc` state), so
the server uses ordinary `tokio::spawn` and `Arc` everywhere — no `!Send`
gymnastics needed there.

---

## Conventions (apply to every step)

- **Derives on new types:** `Serialize, Deserialize, Clone, Debug` always;
  add `Default` where an empty value is meaningful; add `PartialEq` only
  if tests need it.
- **New fields on existing serialized types:** `#[serde(default)]` so older
  on-disk JSON keeps deserializing.
- **Error style:** `Result<T, String>` for `tree_io` and server/CLI call
  sites; `thiserror` enum (`StoreError`) for `Store` in `agent-worker`
  (matches existing pattern). `reqwest::Error` is converted to `LlmError` /
  `ClientError` at the call boundary.
- **Logging:** `log::info!` / `warn!` / `error!`. Prefix multi-component
  logs with a bracketed tag like `[spawner]`, `[worker]`, `[ws]`.
- **Async runtime is `tokio`, `current_thread`.** Each binary's entry point is
  `tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(run())`.
  Follow the Concurrency model above: `!Send` state lives in the `block_on`
  loop; concurrent I/O is `tokio::spawn`ed `Send` forwarder tasks funneling
  into a `tokio::sync::mpsc`. No `LocalSet`/`spawn_local`. No `std::thread::spawn`
  for I/O coordination.
- **Cancellation safety:** never poll a cancel-unsafe future (`read_line`,
  partial `read`) inside `select!`. Use the funnel. `Stream::next` and
  `mpsc::Receiver::recv` *are* cancel-safe, so a `select!` over those is fine
  where a funnel would be overkill (e.g. the CLI loop).
- **Broadcast = drop-and-resync, never stall.** `ServerEvent` fan-out uses
  `tokio::sync::broadcast`. A slow WS client that hits `RecvError::Lagged(n)`
  is **disconnected** (close frame); it reconnects and re-syncs via
  `WsCommand::GetEntries`. INTENTIONAL: we do *not* apply backpressure here —
  one slow client must never stall the worker for every other client. Choose a
  generous channel capacity so lag only happens for genuinely stuck clients.
- **File I/O never blocks the reactor.** Use `tokio::fs` in the core I/O
  layers (`tree_io`, `Store`) *and* in single-file tools (`read`/`write`/
  `edit`/`restore_edit`): `create_dir_all` before writes when the parent dir
  might not exist; atomic `tokio::fs::rename` for any non-append write that
  must not be observed half-written. INTENTIONAL: do **not** run `std::fs`
  inline in a tool — we cannot assume local fast storage (the repo may be on
  SMB/NFS/sshfs, where a "small" read stalls for seconds and would freeze
  streaming, `Stop`, and LSP on the single reactor thread). `tokio::fs` is
  `std::fs` on tokio's blocking pool, so it keeps the reactor free at
  negligible cost for tool-granularity I/O. The exception is *tight-loop*
  traversal (`search`'s `walkdir`+`regex`): wrap the whole sync loop in **one**
  `spawn_blocking` rather than thousands of per-file `tokio::fs` round-trips.
- **Cargo.lock:** never hand-edit. After `Cargo.toml` changes, run
  `cargo build` to regenerate.
- **`#[allow(dead_code)]` is forbidden.** Either use the code or delete it.
- **Tests live with the code:** `#[cfg(test)] mod tests { ... }`. Integration
  tests go in a `tests/` directory at the crate root.
- **Transcribe explanatory comments from the spec into code** — especially
  "INTENTIONAL:" / "DO NOT…" rationale that stops a later reader from
  "improving" the code into a bug.
- **`tree_io` lives in `agent-core`** (async, `tokio::fs`); both the worker's
  `Store` and the server use it for `meta.json`.
- **No C-toolchain TLS (CI).** Fail the build if `openssl-sys` or
  `native-tls` sneak in:
  `! cargo tree -e no-dev -i openssl-sys 2>/dev/null | grep -q openssl-sys`.

---

## Architectural invariants (after all steps complete)

- **Only the worker knows about `data.jsonl`.** `Store` (in `agent-worker`)
  owns all `data.jsonl` I/O via `tokio::fs`. The server and CLI never open it.
- **The server and CLI use `tree_io` directly for `meta.json`.** `tree_io`
  functions are `async` and accept `base: &Path` so they are testable with
  temp dirs and have no hardcoded global state.
- **All outbound HTTP goes through `reqwest`.** There is no `ureq` and no
  hand-rolled TLS anywhere. The provider trait builds request bodies and
  parses SSE; it performs no network I/O. `llm_client` (server-side) is the
  only caller of provider endpoints — the worker is sandboxed and reaches the
  network only via `PipeOut::Llm`.
- **Clients receive entries exclusively over WebSocket.** The REST `/entries`
  endpoint is gone; `WsCommand::GetEntries` asks the worker to replay history.
  Tree CRUD (`/trees`, `/trees/{id}`) stays REST, served by axum; the CLI
  calls it with `reqwest`.
- **Auto-title runs inside the worker.** When `Done` fires, the server sends
  `WsCommand::AutoTitle`; the worker builds the prompt from the conversation it
  already holds, runs it through `WorkerLlmClient::complete` (which marshals
  over the `PipeOut::Llm` → `PipeIn::Llm` pipe transparently), saves the title,
  emits `MetaUpdate`. There is no special turn-engine state for it — it is just
  one more `llm.complete(...).await` (the id-correlated proxy lets it overlap a
  live turn safely). This is **not** about
  avoiding server-side concurrency — with the async event loop the server could
  `tokio::spawn` a title task trivially. It stays in the worker because (1) the
  title prompt needs the conversation, which only the worker reads (the
  `data.jsonl` invariant above), and (2) the worker is sandboxed, so the LLM
  call must route through `llm_client` via the pipe regardless and the worker
  already builds correctly-routed `LlmRequest`s. Generating it server-side would
  force a new pipe round-trip to fetch messages or break the `data.jsonl`
  invariant.

---

## Step template

```
### <Name>

- [ ] todo / - [x] done

**Goal:** one or two sentences.

**Spec:** file paths, signatures, tests, do-not-modify list.

**Verify:** commands that prove it works.
```

On completion: delete this entry, then commit code + PLAN3.md together with:

```
<crate/area>: <brief title>

<what was built, 1-2 sentences>
```

---

## Pending Steps

---

### Phase 1: foundations, reqwest LLM client, async tree_io ✅

- [x] Add `tokio` (`rt`, `macros`, `io-util`, `net`, `process`, `signal`,
      `time`, `sync`, `fs`) to `agent-core`, `agent-server`, `agent-worker`,
      `agent-cli`.
- [x] Add `reqwest` (`default-features = false`, features
      `["rustls-tls", "json", "stream"]`) and `tokio-util` (`io`) to
      `agent-server`.
- [x] Write `agent-server/src/llm_client.rs` — `reqwest` replacement for
      `llm_handler.rs` (515 lines of hand-rolled TLS/chunked/HTTP it deletes).
- [x] Trim the provider trait: keep `build_body` + `parse_stream_event`;
      **delete `Provider::chat` and `OpenAiProvider::chat_raw`** (the two
      `ureq::post` callsites at `provider.rs:190` and `:772`).
- [x] Make `tree_io` functions `async` (backed by `tokio::fs`).
- [x] Write `agent-core/src/child_io.rs` — `ChildLines` over
      `tokio::process::ChildStdout`.

**Goal:** stand up the shared async building blocks; delete the largest block
of hand-written I/O (`LlmHandler`); remove `ureq`.

**Spec:**

`agent-server/src/llm_client.rs` (lives in the server — the only network
caller; the worker proxies via the pipe):
```rust
pub struct LlmClient { http: reqwest::Client }

impl LlmClient {
    pub fn new() -> Self { Self { http: reqwest::Client::new() } } // Arc inside; clone-cheap

    pub async fn stream_completion(
        &self,
        req: &LlmRequest,
        cfg: &ProviderConfig,
        tx: tokio::sync::mpsc::Sender<LlmResponse>,
    ) -> Result<(), LlmError>;

    pub async fn complete(            // non-streaming; used by generate_continuation_brief
        &self,
        req: &LlmRequest,
        cfg: &ProviderConfig,
    ) -> Result<ChatResponse, LlmError>;
}
```
`stream_completion` body shape:
```rust
let resp = self.http.post(cfg.url())
    .headers(provider_auth_headers(cfg))         // x-api-key+anthropic-version, or Bearer
    .json(&provider.build_body(&req.messages, &req.tools, true))
    .send().await?
    .error_for_status()?;                         // maps 4xx/5xx → LlmError

let mut lines = tokio_util::io::StreamReader::new(
        resp.bytes_stream().map_err(io_err))      // reqwest::Error → io::Error
    .lines();                                      // AsyncBufReadExt
while let Some(line) = lines.next_line().await? {
    match provider.parse_stream_event(&line) {     // existing parser, unchanged
        StreamEvent::Chunk(data) => tx.send(LlmResponse::Chunk { id: req.id, data }).await.ok(),
        StreamEvent::Done        => { tx.send(LlmResponse::Done { id: req.id }).await.ok(); break; }
        StreamEvent::Skip        => {}
        // errors → LlmResponse::Error { id, message }
    };
}
```
No TLS setup, no chunked decoder, no `close_notify` — `reqwest` owns all of it.

`agent-server/src/provider.rs`:
- `generate_continuation_brief` becomes `async fn` and calls
  `llm_client.complete(...)`; callers `.await`.

`agent-core/src/child_io.rs`:
```rust
pub struct ChildLines { reader: tokio::io::BufReader<tokio::process::ChildStdout> }
impl ChildLines { pub async fn next_line(&mut self) -> std::io::Result<Option<String>>; }
```

`tree_io` (gain `async`, same returns):
```rust
pub async fn read_meta(base: &Path, id: &str) -> Result<TreeMeta, String>;
pub async fn write_meta(base: &Path, meta: &TreeMeta) -> Result<(), String>; // temp + rename
```

Do not modify `agent-server`'s `worker_loop`/`ws`/`http` beyond making
`generate_continuation_brief` callers await. Delete `llm_handler.rs` only
after Phase 2.

**Verify:**
```
cargo test -p agent-core -p agent-server
# Point stream_completion at a mock HTTP server returning canned SSE; assert
# chunks arrive via the mpsc channel.
! cargo tree -e no-dev -i openssl-sys 2>/dev/null | grep -q openssl-sys
```

---

### Phase 2: agent-server ✅

- [x] Replace `http.rs` + `ws.rs` with an axum `Router`.
- [x] Replace `worker_loop.rs` + `worker_ctx.rs` with an async worker task.
- [x] Replace `ws_client.rs` (the `ws_clients: Vec` + manual `broadcast()`
      loop) with `tokio::sync::broadcast`.
- [x] Replace `llm_handler.rs` calls with `LlmClient::stream_completion`.
- [x] Replace the `mpsc + notify-pipe` wakeup with `tokio::sync::mpsc`.
- [x] Replace `signal_hook` with `tokio::signal`.
- [x] Remove deps: `nix`, `signal-hook`, `tungstenite`, `httparse`, `rustls`,
      `webpki-roots`, `ureq`. Delete files: `http.rs`, `ws.rs`, `ws_client.rs`,
      `worker_loop.rs`, `worker_ctx.rs`, `llm_handler.rs`.

**Goal:** server runs on a `current_thread` tokio runtime via axum; all
hand-written HTTP/WebSocket/TLS code is gone; the only HTTP client is reqwest.

**Spec:**

`agent-server/src/lib.rs` entry point:
```rust
pub fn serve(cfg: Config) -> Result<(), ServerError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all().build()?
        .block_on(run(cfg))
}
```

axum router (`agent-server/src/server.rs`) — note axum 0.8 `/{id}` syntax:
```rust
Router::new()
    .route("/trees",          post(create_tree).get(list_trees))
    .route("/trees/{id}",     get(get_tree).delete(delete_tree))
    .route("/trees/{id}/ws",  get(ws_handler))
    .with_state(app_state)
```
Handlers replace `handlers.rs`/`routes.rs` dispatch boilerplate; business
logic (tree creation, sandbox validation) is unchanged. `AppState` holds
`Arc<Mutex<HashMap<String, WorkerHandle>>>` and a shared `LlmClient` and
`Arc<Config>`. `WorkerHandle` carries `cmd_tx: mpsc::Sender<WsCommand>` and
`ev_tx: broadcast::Sender<ServerEvent>`.

`agent-server/src/worker_task.rs` — replaces `worker_loop.rs`:
```rust
pub async fn run_worker_task(
    tree_id: String,
    mut child: tokio::process::Child,
    cfg: Arc<Config>,
    llm: LlmClient,
    cmd_rx: mpsc::Receiver<WsCommand>,
    ev_tx: broadcast::Sender<ServerEvent>,
);
```
- A forwarder task reads child stdout via `ChildLines::next_line` and funnels
  `WorkerEvent::Stdout(PipeOut)` into an internal `mpsc`; the main loop
  `recv()`s a unified `WorkerEvent` (stdout | command | child-exit).
- `PipeOut::Event(ev)` → `let _ = ev_tx.send(ev);` (broadcast: ignore "no
  receivers"; lagging is handled receiver-side).
- `PipeOut::Llm(req)` → `tokio::spawn(llm.clone().stream_completion(req,
  &cfg.provider, pipe_in_tx))`. reqwest futures are `Send`, so plain `spawn`
  works — no `LocalSet`. The task writes `PipeIn::Llm(resp)` back to the
  worker's stdin pipe.
- On child stdout EOF + non-zero exit, broadcast
  `ServerEvent::Done { status: "aborted" }` (same crash detection as today).

`agent-server/src/ws_handler.rs` (axum WS):
```rust
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, id, state))
}

async fn handle_ws(mut socket: WebSocket, tree_id: String, state: AppState) {
    let (cmd_tx, mut ev_rx) = state.get_or_spawn_worker(&tree_id).await; // ev_tx.subscribe()
    // INTENTIONAL: subscribe (ev_rx) is taken BEFORE issuing GetEntries. If we
    // replayed first and subscribed second, events emitted during replay would
    // be lost. Subscribing first means the live stream may overlap the replayed
    // tail; the client de-dupes by entry id, so overlap is harmless, a gap is not.
    cmd_tx.send(WsCommand::GetEntries { count: None }).await.ok();

    loop {
        tokio::select! {                          // both branches are cancel-safe
            msg = socket.recv() => match msg {
                Some(Ok(m))  => { /* parse WsCommand → cmd_tx.send */ }
                _            => break,             // client closed
            },
            ev = ev_rx.recv() => match ev {
                Ok(ev)                      => { socket.send(serialize(ev)).await.ok(); }
                Err(RecvError::Lagged(_))   => break,   // drop-and-resync: client reconnects
                Err(RecvError::Closed)      => break,
            },
        }
    }
}
```
This replaces `ws_clients: Vec<WsClient>` + manual `broadcast()` and the
`NotifyHandler`/`notify_read`/`notify_write` pipe wakeup entirely.

SIGINT/SIGTERM: a small task selecting over `tokio::signal::ctrl_c()` and
`tokio::signal::unix::signal(SignalKind::terminate())` triggers graceful
shutdown, replacing `signal_hook::flag::register`.

**Verify:**
```
cargo build -p agent-server
cargo test -p agent-server
# Start server; connect two websocat clients; send a message; confirm BOTH
# receive the streamed events.
! cargo tree -e no-dev -i openssl-sys 2>/dev/null | grep -q openssl-sys
```

---

### Phase 3: agent-worker — async loop + linear turn engine

- [ ] Wrap `run()` in a `current_thread` runtime + `block_on(run_async())`.
- [ ] Replace the `nix::poll` loop with the event-funnel pattern: `tokio::spawn`
      forwarder tasks for async stdin, each LSP child stdout, and a
      `tokio::signal` shutdown task, all into `mpsc`s.
- [x] Add `agent-worker/src/llm.rs` — **`WorkerLlmClient`**, a pipe proxy that
      hides `PipeIn`/`PipeOut` behind a streaming async API, correlating by the
      existing `LlmRequest.id`.
- [ ] **Rewrite `turn.rs` / `agent.rs`**: delete the explicit `AgentState`
      state machine and replace it with a linear async `run_turn` that calls
      `WorkerLlmClient` directly. Cancellation = dropping the turn future.
- [ ] Auto-title becomes `llm.complete(prompt).await?` — delete the
      `AutoTitling` state.
- [x] Make the `Tool` trait async (`#[async_trait]`, keep the `Tool: Send`
      bound): `async fn execute(&self, params, ctx: &mut ToolContext) -> ToolOutput`.
- [ ] Rewrite `bash.rs` on `tokio::process::Command` (`.kill_on_drop(true)`,
      async stdout/stderr, `tokio::time::timeout`); delete its two `std::thread`
      readers + the watcher thread + the `ctx.stop`/`SIGTERM_RECEIVED` polling.
- [ ] Move single-file tools (`read`/`write`/`edit`/`restore_edit`) to
      `tokio::fs`; wrap `search`'s traversal in one `spawn_blocking` that keeps
      polling `ctx.stop`.
- [ ] Rewrite `lsp_client.rs` with `tokio::process::Command` + async I/O;
      remove all `nix::fcntl`, `IntoRawFd`, manual `read()`.
- [ ] Replace the `ctrlc` crate as the `SIGTERM_RECEIVED` source with a
      `tokio::signal` task (keep the `AtomicBool` for cooperative cancel in tools).
- [ ] (Optional) Add `PipeOut::CancelLlm { id }`; the server aborts the
      in-flight stream task on receipt (see below).
- [ ] Delete `read_stdin_into_buf`, `parse_pipe_messages`, `O_NONBLOCK` setup,
      the `AgentState` enum; remove `nix` and `ctrlc`.

**Goal:** the worker is a clean async event loop **and** the agent logic reads
as linear async code — `loop { stream…; run_tools(); }` — with no hand-cranked
turn state machine and no visible pipe marshalling.

**Spec:**

`agent-worker/src/llm.rs` — the proxy the agent author actually uses:
```rust
pub struct WorkerLlmClient {
    pipe_tx: mpsc::Sender<PipeOut>,                               // → single stdout writer task
    pending: Rc<RefCell<HashMap<u64, mpsc::Sender<LlmResponse>>>>,
    next_id: Cell<u64>,
}

impl WorkerLlmClient {
    pub fn request(&self, messages: Vec<Message>, tools: Vec<ToolDefinition>,
                   routing: Option<String>) -> LlmStream;          // streaming
    pub async fn complete(&self, messages: Vec<Message>,
                          tools: Vec<ToolDefinition>) -> Result<ChatResponse, LlmError>; // one-shot
    pub(crate) fn route(&self, resp: LlmResponse);                 // called by the main loop
}

pub struct LlmStream { /* id, rx: mpsc::Receiver<LlmResponse>, builder, Weak<pending> */ }
impl futures::Stream for LlmStream { type Item = Result<String, LlmError>; /* yields text */ }
impl LlmStream { pub fn finish(self) -> ChatResponse; }            // tool_calls / usage / stop_reason
impl Drop for LlmStream { /* deregister id from pending; optionally send PipeOut::CancelLlm */ }
```
- `request`: `id = next_id`; make `(tx, rx)`; `pending.insert(id, tx)`;
  `pipe_tx.send(PipeOut::Llm(LlmRequest { id, messages, tools, routing_id }))`;
  return `LlmStream`.
- `route`: `pending.get(&resp.id())` → forward; on `Done`/`Error` remove the entry.
- `complete`: builds a `request`, drains it, returns the assembled `ChatResponse`
  (used by auto-title and summaries).

INTENTIONAL: writes to stdout go through one writer task fed by `pipe_tx`, so
the proxy never touches `stdout` directly and concurrent requests can't
interleave bytes.

`agent-worker/src/agent.rs` — the entire turn engine (replaces `AgentState`):
```rust
async fn run_turn(llm: &WorkerLlmClient, ctx: &Rc<RefCell<TurnCtx>>, user_msg: String)
    -> WorkerResult<()>
{
    ctx.borrow_mut().push_user(user_msg);
    loop {
        let (msgs, tools, routing) = ctx.borrow().snapshot();
        let mut stream = llm.request(msgs, tools, routing);
        while let Some(chunk) = stream.next().await {
            let text = chunk?;
            let mut c = ctx.borrow_mut();
            c.append_assistant(&text);
            c.emit(ServerEvent::Chunk { /* live tokens */ });
        }
        let resp = stream.finish();
        ctx.borrow_mut().record_assistant(&resp);
        match resp.tool_calls {
            None => break,                                   // turn complete
            Some(calls) => {
                let results = run_tools(calls, ctx).await;    // async; see Tool execution below
                ctx.borrow_mut().record_tool_results(results);
                // loop → next llm.request() with tool results appended
            }
        }
    }
    Ok(())
}
```

`agent-worker/src/lib.rs` — main loop drives the turn *concurrently with* the
pipe drain, so chunks arrive while the turn is parked at an `await`:
```rust
pub fn run() -> WorkerResult<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?
        .block_on(run_async())
}

async fn run_async() -> WorkerResult<()> {
    let (pipe_tx, _) = /* stdout writer task */;
    let llm = WorkerLlmClient::new(pipe_tx.clone());
    let ctx = Rc::new(RefCell::new(TurnCtx::new(/* emit via pipe_tx */)));
    let mut turn: Option<Pin<Box<dyn Future<Output = WorkerResult<()>>>>> = None;

    tokio::spawn(stdin_forwarder(pipein_tx.clone()));   // Send forwarder → PipeIn
    // + lsp forwarders, signal forwarder (as before)

    loop {
        tokio::select! {
            Some(p) = pipein_rx.recv() => match p {
                PipeIn::Llm(resp)                 => llm.route(resp),         // wakes the stream
                PipeIn::Cmd(WsCommand::Message{params}) =>
                    turn = Some(Box::pin(run_turn(&llm, &ctx, params.text))),
                PipeIn::Cmd(WsCommand::Stop)      => { turn = None;           // drop == cancel
                                                       ctx.borrow().emit(Done{status:"stopped"}); }
                PipeIn::Cmd(WsCommand::AutoTitle) => {
                    let t = llm.complete(ctx.borrow().title_prompt(), vec![]).await?;
                    tree_io::write_meta(/* …title… */).await?;
                    ctx.borrow().emit(MetaUpdate);
                }
                PipeIn::Cmd(WsCommand::GetEntries{..}) => replay_history(&ctx).await,
                PipeIn::Config(cfg)               => ctx.borrow_mut().apply_config(cfg),
            },
            Some(res) = poll_opt(&mut turn) => { emit_done(&ctx, res); turn = None; }
            Some(lsp) = lsp_rx.recv()       => handle_lsp_data(lsp, &ctx),
            _ = &mut shutdown               => break,
        }
    }
    Ok(())
}
```

INTENTIONAL (transcribe into code):
- **Cancellation is `turn = None`.** Dropping the turn future unwinds it at its
  current `await`; the `LlmStream` it holds is dropped too, which deregisters
  its id (and optionally sends `PipeOut::CancelLlm`). There is no
  `AgentState` flag to check and no half-states to reconcile — this is the
  whole reason the state machine is gone.
- **The turn future is NOT `tokio::spawn`ed.** It holds `Rc`/`!Send` state, so
  it lives in the `block_on` loop and is polled via `select!` (the
  block_on-vs-spawn split from the Concurrency model). Only the `Send`
  stdin/LSP/signal forwarders are spawned.
- **`ctx` is `Rc<RefCell<TurnCtx>>`**, shared between the loop and the turn
  future (single-threaded → no `Mutex`). `&llm` is shared by `request`/`route`
  (both `&self`), so the loop routing a response and the turn issuing a request
  never alias mutably.
- **`read_line` is never inside a `select!`** — the dedicated stdin forwarder
  owns the reader and emits only complete lines (cancel-safety; see Conventions).

`agent-worker/src/lsp_client.rs` — `LspClient::spawn`:
- `tokio::process::Command` with `.stdout(Stdio::piped())`; store
  `tokio::process::ChildStdout` (no raw fd). A per-client forwarder task reads
  framed LSP messages and sends them into `lsp_tx`.
- `write_request` may use `AsyncWriteExt::write_all` (writes are small).
- LSP wait deadlines: instead of a separate timer event, the turn (or an LSP
  helper) `await`s `tokio::time::sleep_until(deadline)` inline now that flow is
  linear.

**Tool execution.** The `Tool` trait becomes async — uniform `dyn` dispatch
means the *signature* is uniformly async; the *body* of each tool picks a
strategy. (`#[async_trait]`, because native async-fn-in-trait isn't
`dyn`-compatible; the existing `Tool: Send` bound stays.)
```rust
#[async_trait]
pub trait Tool: Send {
    async fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput;
}
```
Three implementation strategies behind that one signature:
- **`bash` → fully async.** `tokio::process::Command` with `.kill_on_drop(true)`,
  async stdout/stderr capture, `tokio::time::timeout` for the deadline. This
  **deletes** today's two `std::thread` readers + the watcher thread. Cancellation
  is now structural: `Stop` drops the turn future → drops the bash future →
  `kill_on_drop` SIGKILLs the child. No `ctx.stop` polling needed here.
- **`search` → `spawn_blocking`.** The `walkdir`+`regex` traversal is many fs
  ops in a tight loop, so wrap the whole sync loop in one `spawn_blocking(...)`
  and `.await` it. The closure is `Send + 'static`, so it cannot borrow `ctx`
  (`Rc`/`!Send`): pass owned args in, return an owned `ToolOutput`. Blocking-pool
  work is **not** drop-abortable, so it keeps polling `ctx.stop` to be
  cancellable.
- **`read`/`write`/`edit`/`restore_edit` → `tokio::fs`.** Single-file ops; an
  `async fn` body that `await`s `tokio::fs`. Do not run `std::fs` inline (SMB/NFS
  stalls would freeze the reactor — see the File I/O convention).

`ctx.stop: Arc<AtomicBool>` is **retained** but its role narrows: it is now only
the cooperative-cancel signal for `spawn_blocking` tools (the signal forwarder
flips it on SIGTERM, and the turn-cancel path can flip it too). Async tools
(`bash`, `tokio::fs`) get cancellation from future-drop instead.

Tool calls within a turn run **sequentially** (`for call in calls { …await… }`),
not concurrently: `ctx` is `Rc<RefCell<…>>` (can't be shared across concurrent
futures) and concurrent fs edits would race. `join_all` is intentionally avoided.

**Optional `PipeOut::CancelLlm { id }`** (true upstream cancellation): add the
variant to `rpc.rs`; the server's `worker_task` keeps a
`HashMap<u64, JoinHandle>` of in-flight `stream_completion` tasks and calls
`.abort()` on receipt. Without it, a cancelled turn's chunks are simply
discarded by the (now-removed) registry entry — correct, but the server keeps
streaming to the provider until `Done`.

**Verify:**
```
cargo build -p agent-worker
cargo test -p agent-worker
# End-to-end: server + worker, send a message, confirm response streams back.
# Cancellation: send Message then Stop mid-stream; assert the turn unwinds,
#   no further Chunk events arrive, and the pending registry is empty.
# Auto-title: assert llm.complete() returns a title while a turn is idle.
# Bash cancel: run a long bash command, then Stop; assert the child is killed
#   (rewrite of test_bash_cancels_on_stop_flag for kill_on_drop semantics).
# Reactor liveness: while a long bash/search tool runs, assert Chunk/LSP events
#   from a concurrent path still flow (proves the reactor isn't blocked).
```

---

### Phase 4: agent-cli

- [ ] Wrap the CLI entry in a `current_thread` runtime + `block_on`.
- [ ] Replace the `nix::poll` loops in `interactive.rs` with `tokio::select!`
      over cancel-safe sources (terminal `EventStream`, WS stream, render tick).
- [ ] Replace `tungstenite` (blocking) with `tokio-tungstenite::connect_async`.
- [ ] Replace `ureq` REST calls in `client.rs` with `reqwest`.
- [ ] Replace the embedded-server socketpair (`local.rs`) — see below.
- [ ] Use `crossterm::event::EventStream` (feature `event-stream`).
- [ ] Remove deps: `nix`, `ctrlc`, `tungstenite`, `ureq`.

**Goal:** the CLI event loop is a single `tokio::select!` over terminal events,
WebSocket messages, and the 16 ms render tick; no `ureq`, no `nix`.

**Spec:**

`agent-cli/src/client.rs` (tree CRUD over reqwest):
```rust
pub async fn list_trees(&self, base: &str) -> Result<Vec<TreeMeta>, ClientError> {
    Ok(self.http.get(format!("{base}/trees")).send().await?.error_for_status()?.json().await?)
}
// create/get/delete likewise; reqwest::Error → ClientError via From.
```

`agent-cli/src/interactive.rs` main loop (these sources are all cancel-safe, so
a direct `select!` is fine — no funnel needed):
```rust
let mut term = crossterm::event::EventStream::new();
let mut tick = tokio::time::interval(Duration::from_millis(16));
loop {
    tokio::select! {
        Some(ev) = term.next()  => handle_term_event(ev?, &mut state),
        msg      = ws.next()    => match msg { Some(m) => handle_ws_msg(m?, &mut state), None => break },
        _        = tick.tick()  => render(&mut terminal, &state)?,
    }
}
```
`ws` is a `tokio_tungstenite::WebSocketStream` (`.next()` yields
`tungstenite::Message`).

Connection setup:
- Remote: `tokio_tungstenite::connect_async(url)`.
- Embedded: the in-process server is reachable over loopback TCP, so the CLI
  connects with `connect_async("ws://127.0.0.1:{port}/…")` like the remote
  path — this **deletes the socketpair (`nix::socket`) entirely** rather than
  porting it. (If a non-TCP transport is still wanted, use
  `tokio::net::UnixStream::pair()` and `client_async` over one half.)

The `poll_event` wrapper around `crossterm::event::poll` in `app.rs`/`tui.rs`
is removed; events flow through `EventStream`.

**Verify:**
```
cargo build -p agent-cli
# Launch CLI against a running server; confirm TUI renders, input works,
# streaming responses display; confirm embedded-server mode works.
```

---

### Subworker support

- [ ] Add `PipeOut::SpawnSubWorker` and `PipeIn::SubWorkerDone` variants.
- [ ] Add `ServerEvent::{SubWorkerSpawned, SubWorkerDone, Sub}` variants.
- [ ] Server: handle `SpawnSubWorker`, spawn child with copied sandbox, enforce
      a depth/fan-out cap.
- [ ] Server: GC child workers when parent exits.
- [ ] Worker: implement `spawn_subworker` in `agent.rs` / a tool.

**Goal:** a worker can delegate a subtask to a child worker under the same
sandbox; the parent is notified on completion; the client sees sub-worker
events in the parent's stream.

**Spec:**

Reuse the **existing** `Tree.parent_id` field (`types.rs:34`) for the persisted
child→parent link — do **not** add a second `parent_tree_id` to the on-disk
meta. The in-memory `WorkerHandle` carries `parent_tree_id` for GC bookkeeping.

New `PipeOut` (worker → server):
```rust
SpawnSubWorker { sub_tree_id: String, message: String, repo_path: Option<PathBuf> }
```
New `PipeIn` (server → parent worker):
```rust
SubWorkerDone { sub_tree_id: String, status: String, summary: String } // summary ≤ 2 KB
```
New `ServerEvent` (in the parent's stream):
```rust
SubWorkerSpawned { sub_tree_id: String },
SubWorkerDone    { sub_tree_id: String, status: String },
Sub              { id: String, event: Box<ServerEvent> },
```
Clients wanting the full sub-stream can also open `/trees/{sub_tree_id}/ws`.

**Depth / fan-out cap (prevents unbounded recursion):**
```rust
const SUBWORKER_MAX_DEPTH: u32 = 3;
const SUBWORKER_MAX_CHILDREN: usize = 8; // per parent
```
`WorkerHandle` gains `depth: u32` (root = 0). On `SpawnSubWorker`,
`child_depth = parent.depth + 1`; if it exceeds `SUBWORKER_MAX_DEPTH`, or the
parent already has `SUBWORKER_MAX_CHILDREN` live children, the server refuses:
send `PipeIn::SubWorkerDone { status: "error", summary: "depth/fan-out limit
exceeded", .. }` and broadcast nothing else.

Spawn logic (`agent-server/src/spawner.rs`), on `SpawnSubWorker`:
1. Enforce the cap.
2. Look up the parent's `sandbox` from `meta.json`.
3. Create a tree record for `sub_tree_id` with `parent_id = Some(parent_id)`
   and `sandbox` copied verbatim.
4. Spawn a worker for `sub_tree_id` via the existing path, recording `depth`
   and `parent_tree_id` on its `WorkerHandle`.
5. Send `WsCommand::Message { params: { text: message } }` to the sub-worker.
6. Broadcast `ServerEvent::SubWorkerSpawned { sub_tree_id }` to the parent.

Sub-worker events broadcast on the sub-worker's own `ev_tx` AND are forwarded
to the parent's `ev_tx` wrapped in `ServerEvent::Sub { id, event }`. On
sub-worker exit: send `PipeIn::SubWorkerDone` to the parent's stdin and
broadcast `ServerEvent::SubWorkerDone` to the parent's clients.

GC (`agent-server/src/spawner.rs`):
```rust
// Called from run_worker_task on child process exit, lock held:
fn reap_worker(tree_id: &str, active: &mut HashMap<String, WorkerHandle>) {
    active.remove(tree_id);
    let children: Vec<_> = active.values()
        .filter(|w| w.parent_tree_id.as_deref() == Some(tree_id))
        .map(|w| w.tree_id.clone())
        .collect();
    for child_id in children {
        if let Some(w) = active.get_mut(&child_id) {
            // tokio::process::Child::kill() is ASYNC; start_kill() sends SIGKILL
            // synchronously, which is why reap_worker can stay non-async with the
            // lock held. DO NOT switch this to .kill().await here.
            let _ = w.child.start_kill();
            w.task.abort();      // abort the JoinHandle for its worker task
        }
        reap_worker(&child_id, active);
    }
}
```

The sandbox copy is not re-validated at sub-worker spawn time. The parent's
sandbox was validated at parent tree creation and is immutable; copying it
directly is equivalent to OS inheritance for policy purposes.

**Verify:**
```
cargo test -p agent-server
# GC unit test: spawn parent + child, kill parent, assert child is reaped.
# Cap unit test: request a 4th-level sub-worker, assert it is refused with
#   status "error" and no process is spawned.
# Integration: trigger SpawnSubWorker; confirm SubWorkerSpawned on WS, the
#   sub-worker runs, SubWorkerDone arrives.
```

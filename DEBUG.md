# Debugging Guide

Quick reference for debugging the agent-server and CLI.

## Running the Stack

```bash
# Terminal 1 — Server with full logs
cd /home/robin/Code/agent
export LLM_API_KEY="sk-or-v1-..."
export AGENT_BASE_URL="https://openrouter.ai/api/v1"
export AGENT_MODEL="deepseek/deepseek-v4-flash"
export RUST_LOG=info
cargo run -p agent-server

# Terminal 2 — Send a one-shot message
export LLM_API_KEY="sk-or-v1-..."
export AGENT_BASE_URL="https://openrouter.ai/api/v1"
export AGENT_MODEL="deepseek/deepseek-v4-flash"
cargo run -p agent-cli msg <tree-uuid> "your message"

# List / create trees
cargo run -p agent-cli trees
cargo run -p agent-cli create "my tree title"
```

## Diagnosing SSE Streaming Issues

### Step 1 — Check raw SSE from OpenRouter

Look for "SSE raw:" lines in the server log. They show every SSE line from the
provider (truncated to 500 chars). This is the most useful signal:

```
[INFO agent_core::agent] SSE raw: data: {"id":"gen-...","choices":[{"index":0,"delta":{"content":"Hello",...}}]}
[INFO agent_core::agent] SSE raw: data: [DONE]
[INFO agent_core::agent] SSE stream ended ([DONE])
```

If you see `[DONE]` but the CLI got no output → skip to Step 3.

If you see no SSE raw lines at all → the provider HTTP call failed (check
`AGENT_BASE_URL`, `LLM_API_KEY`, network).

### Step 2 — Test SSE streaming independently with curl

```bash
# Terminal 1 — Start SSE stream (this auto-spawns the agent, waiting for input)
curl -v -N "http://localhost:8080/trees/<tree-uuid>/stream"

# Terminal 2 — Send message
curl -s -X POST "http://localhost:8080/trees/<tree-uuid>/message" \
  -H "Content-Type: application/json" \
  -d '{"text":"say hello"}'
```

Expected curl output:
```
< HTTP/1.1 200 OK
< Content-Type: text/event-stream
< Transfer-Encoding: chunked
<
data: {"type":"text_chunk","content":"Hello"}
data: {"type":"done","status":"stop"}
...
```

If curl hangs with no data → the `SseUpgrade` / `Upgrade` mechanism isn't
sending data (check `[sse-upgrade]` logs).

If curl shows `HTTP/1.1 200 OK` but stalls at 0 bytes → tiny_http's BufWriter
is buffering (this was the root cause — fixed by using `rouille::Upgrade`
instead of `ResponseBody::from_reader`).

### Step 3 — Check `[sse]` logs

The SSE handler logs every step. Enable `RUST_LOG=info` and grep for `[sse]`:

```
[sse] Opening SSE stream for tree <uuid>
[sse] Auto-spawning agent for tree <uuid>
[sse] Using SSE upgrade for tree <uuid>
[sse-upgrade] Starting SSE write loop for tree <uuid>
```

If you see "Auto-spawning" but curl still gets 404 → the spawn failed (check
`[lifecycle]` logs).

If you see "Starting SSE write loop" but no data reaches curl → the write loop
is stuck (check bridge logs).

### Step 4 — Check bridge & agent lifecycle

```
[lifecycle] Spawned agent + bridge for tree <uuid>
[lifecycle] Bridge thread exited for tree <uuid>
[lifecycle] Agent thread exited for tree <uuid>
```

If agent exits before SSE stream opens → race condition (fixed by auto-spawn
in `handle_sse_stream`). Verify the CLI opens the SSE stream FIRST.

If bridge never exits → the `event_tx` channel never closed (agent still running
or blocked on LLM call). Check for "Agent loop started" and "Processing message"
logs.

## Common Problems

### CLI hangs with no output
Most likely cause: SSE stream opens but the BufWriter buffers everything.
**Fix:** The `SseUpgrade` upgrade mechanism flushes after every event. If it's
still happening, check that `rouille::Upgrade` is being used (not
`ResponseBody::from_reader`).

### "Read-only file system (os error 30)" when writing to store
The root filesystem `/` is btrfs and mounted `ro`. `~/.agent` inherits this.
Workaround (already applied):
```bash
export AGENT_DIR="/home/robin/Code/agent/.agent-data"
mkdir -p "$AGENT_DIR/trees"
```
Or verify bind mounts are active: `mount | grep "/home/robin/.agent"`.

### CLI shows warnings: "failed to parse SSE event"
The client received non-JSON data on the SSE stream. This happens when
`SseUpgrade` writes raw HTTP headers alongside event data. The fix is the
`headers_written` flag — headers should only come from the initial upgrade
response, not duplicated in `build()`. If warnings appear, check that
`headers_written: true` is set on the `SseUpgrade` struct and that no
manual headers are written in `build()`.

### "No active agent for tree ..."
The SSE stream opened but the agent already exited. The CLI now opens the
stream FIRST (which auto-spawns), then sends the message. If this error
still appears, check: is the agent spawning? (look for `[lifecycle] Spawned`
logs). Is the tree valid? (`cargo run -p agent-cli trees`).

## Architecture (Data Flow)

```
CLI                          Server
 │                            │
 ├── GET /stream ─────────────┤  Opens SSE first, auto-spawns agent
 │                            ├── SseUpgrade::build() gets raw socket
 │                            │   writes HTTP 200 + SSE headers
 │                            │   subscribes to event broadcast
 │                            │
 ├── POST /message ───────────┤  Sends message to agent via channel
 │                            ├── agent → LLM → events → bridge
 │                            │   bridge → event_broadcast → SseUpgrade
 │                            │   SseUpgrade flushes each event to socket
 ◄══ data: {text_chunk} ══════╪══ Real-time streaming to CLI
 ◄══ data: {done} ════════════╪══
```

## Key Files

| File | Purpose |
|------|---------|
| `agent-cli/src/main.rs` | CLI entry point. `send_and_stream()` opens SSE first, sends message second |
| `agent-cli/src/client.rs` | HTTP client. `SseEventStream::next_event()` parses SSE |
| `agent-server/src/routes.rs` | HTTP handlers. `SseUpgrade` struct does direct socket writes |
| `agent-server/src/lifecycle.rs` | Agent lifecycle. Bridge buffers ALL events, clears broadcast on exit |
| `agent-core/src/agent.rs` | Agent loop. `run_agent()` processes messages, emits `ServerEvent`s |
| `agent-core/src/provider.rs` | LLM provider. `stream_chat()` sends HTTP request, returns chunk stream |
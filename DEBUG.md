# Debugging Guide

Quick reference for debugging the agent server, worker, and CLI.

## Running the stack

```bash
# Build everything (creates target/debug/agent — one binary, three subcommands)
cargo build --workspace

# Terminal 1 — Server with info logs to stdout + /tmp/agent-server.log
export LLM_API_KEY="sk-..."
export AGENT_BASE_URL="https://openrouter.ai/api/v1"
export AGENT_MODEL="anthropic/claude-3.5-sonnet"
export RUST_LOG=info
./target/debug/agent server

# Terminal 2 — interactive TUI (default subcommand)
./target/debug/agent cli

# Terminal 2 — one-shot send and stream
./target/debug/agent cli msg <tree-uuid> "your message"

# Terminal 2 — list / create trees (HTTP only, no worker spawn)
./target/debug/agent cli trees
./target/debug/agent cli create "title" --repo-path ~/Code/myrepo
```

## What lives where

| Thing | Location |
|---|---|
| Code | `agent-core/`, `agent-server/`, `agent-worker/`, `agent-cli/`, `agent/` |
| Config (optional) | `~/.agent/config.toml` — missing file means defaults |
| Tree data | `~/.agent/trees/{uuid}/data.jsonl` (header + entries, append-only) |
| Tree metadata | `~/.agent/trees/{uuid}/meta.json` (server-only writer) |
| Server log | `/tmp/agent-server.log` + stderr (set via `[logging]`) |
| Plan | `SANDBOX.md` (steps + notes) |

## Architecture cheat sheet

```
CLI (tungstenite client) ── WS ──▶ Server (one TcpListener, one port)
                                       │
                                       ├── HTTP routes (tree CRUD)
                                       │
                                       └── WS upgrade per tree
                                              │
                                              ▼
                                       Worker subprocess (bwrap-sandboxed)
                                       stdin: JSON commands
                                       stdout: JSON ServerEvents
                                       stderr: demuxed to server log
```

- One worker process per active tree, lives across multiple turns
- WS thread on the server owns the socket exclusively (non-blocking + 10ms poll)
- Stdout proxy thread reads worker events, fans out to subscribers + ring buffer
- bwrap argv built from per-tree `TreeSandbox` config + global `[sandbox.defaults]`

## Quick diagnosis: "Fatal: worker exited unexpectedly"

This is the message the CLI shows when the worker subprocess died. **First place to look is the server log** — `[worker {short-id}] ...` lines are the worker's stderr.

### bwrap failures (most common)

```
[worker abcd...] bwrap: Can't mkdir /home/user/.bash_history: Not a directory
```

The hide list contained a file. Fix per Step 12 in `SANDBOX.md`: emit `--bind /dev/null <path>` for files, `--tmpfs <path>` for directories.

```
[worker abcd...] bwrap: setting up uid map: Permission denied
```

User namespaces aren't enabled in the kernel. Either enable
`kernel.unprivileged_userns_clone=1` or set `sandbox.enabled = false` in config.

```
[worker abcd...] bwrap: Can't bind mount /home/user/repo on /home/user/repo: No such file or directory
```

`repo_path` in the tree's meta.json points at a missing directory. Fix the meta or recreate the tree.

### Worker panics inside the agent loop

Look for backtrace lines following the worker's stderr. Often a tool execution error or provider issue. Test the worker in isolation:

```bash
echo '{"method":"message","params":{"text":"list files"}}' \
  | ./target/debug/agent worker --tree-id <id> --config ~/.agent/config.toml
```

Direct invocation skips bwrap and shows the full stderr.

### Provider misconfiguration

Default provider points at `http://localhost:8080/v1` (the server itself). If `AGENT_BASE_URL` / `LLM_API_KEY` aren't set, the LLM call fails and the agent emits a fatal Error event before exiting. Worker exits cleanly (status 0), so this shows as `Error: ...` not `worker exited unexpectedly`.

## Inspecting a tree's state

```bash
# Raw entries (one JSON object per line, header on line 1)
cat ~/.agent/trees/<uuid>/data.jsonl | jq

# Metadata (atomic-written, server-only)
cat ~/.agent/trees/<uuid>/meta.json | jq

# Server's view via HTTP
curl -s http://localhost:8080/trees/<uuid> | jq
curl -s http://localhost:8080/trees/<uuid>/entries | jq
```

## Testing the WS layer directly

```bash
# Requires `pip install websocket-client` or use wscat
python3 -c "
import websocket, json
ws = websocket.create_connection('ws://localhost:8080/trees/<uuid>/ws')
ws.send(json.dumps({'method':'message','params':{'text':'hi'}}))
while True:
    print(ws.recv())
"
```

If the WS connection drops immediately, the worker spawn failed — check the server log for `[lifecycle]` and `[worker]` lines.

## SIGINT / clean shutdown

`Ctrl-C` on the server triggers `signal_hook` → `shutting_down` flag → main accept loop breaks → `lifecycle::shutdown_all` walks active workers, sends `{"method":"stop"}` on each stdin, polls `try_wait` for up to 60s, escalates to `child.kill()` for stragglers, then `recover_tree` appends a linked synthetic `SessionEnd` (status `Aborted`) so the next session boundary is clean.

If the server is killed with SIGKILL (no graceful shutdown), `--die-with-parent` on bwrap kills the workers, and on next startup `scan_unterminated` writes synthetic `SessionEnd`s for any tree whose last entry isn't one.

## Integration test (Step 10)

The end-to-end test spawns a real `agent worker` subprocess with `AGENT_TEST_STUB=1` set, which makes `provider.stream_chat()` return canned SSE data instead of hitting a real API. Useful for validating the full RPC roundtrip:

```bash
cargo test -p agent-worker --test end_to_end
```

If you want to manually exercise the stub path:

```bash
AGENT_TEST_STUB=1 ./target/debug/agent worker --tree-id <id> --config <path>
```

## Common log tags

| Tag | Source | What it means |
|---|---|---|
| `[lifecycle]` | server | worker spawn / shutdown / recover |
| `[proxy <id>]` | server stdout-proxy thread | worker stdout reader |
| `[worker <id>]` | server stderr-demux thread | a line the worker wrote to stderr |
| `[ws <id>]` | server WS session thread | per-connection events (pong timeout, etc.) |
| `[auto-title <id>]` | server side thread | LLM-driven title generation after first turn |

## Sandbox toggle for development

When iterating on something that's hard to debug inside bwrap (e.g., a tool that wants to spawn a subprocess), set `sandbox.enabled = false` in `~/.agent/config.toml`:

```toml
[sandbox]
enabled = false
```

The worker spawns directly via `Command::new("agent")` with no namespace isolation. All other behavior is identical.

## "I deleted ~/.agent but trees still show up"

The CLI's interactive tree picker pulls from the server's in-memory `INDEX_CACHE`, which is populated at startup from `~/.agent/trees/*/meta.json`. Restart the server after deleting tree directories.

## Legacy flat-layout files in ~/.agent/trees/

Pre-restructure trees were stored as `~/.agent/trees/{id}.jsonl` + `{id}.meta.json` (flat). The current code uses `~/.agent/trees/{id}/data.jsonl` + `meta.json` (subdirectory). There's no migration — delete the old flat files manually:

```bash
find ~/.agent/trees -maxdepth 1 -type f -delete
```

The subdirectories are untouched.

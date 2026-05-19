# Permission/Confirmation System Plan

## Goal

Require Y/n confirmation for all potentially destructive tools (bash, write, edit, git add/commit/push/pull) with a common mechanism that can be:

- **Global off switch** — one flag to disable all prompts
- **Fine-grained rules** — allow/deny/prompt by tool, subcommand, or pattern
- **Transparent to tools** — no changes to individual tool implementations

---

## Architecture

### Permission Layers (checked in order, first match wins)

```
Hook check → Permission check → Tool execution
                ↑
         PermissionRegistry
         (global rules + tool-specific rules)
```

1. **Hooks** (existing, unchanged) — `Block` or `PassThrough` based on content (e.g. `rm -rf`)
2. **Permission check** (new) — consult `PermissionRegistry` for the tool/subcommand/args
3. **Tool execution** — only reached if both hook and permission pass

### Permission Levels

```rust
enum PermissionLevel {
    Allow,         // always execute without prompting (default for read-only tools)
    Deny,          // always block without prompting
    Prompt,        // ask user for Y/n confirmation
}
```

### PermissionRegistry

Central rules engine, stored on `AgentState` and passed through to the agent loop.

```rust
struct PermissionRegistry {
    default_level: PermissionLevel,         // Prompt (default)
    tool_rules: HashMap<String, PermissionLevel>,  // tool name → level
    subcommand_rules: HashMap<(String, String), PermissionLevel>, // (tool, subcmd) → level
    glob_rules: Vec<(GlobPattern, PermissionLevel)>,  // arg pattern → level
}
```

Built-in defaults:

| Tool / Pattern | Level |
|---------------|-------|
| read, ls, grep, find, search_messages, search_files | Allow |
| git: status, diff, log, show | Allow |
| write, edit, bash | Prompt |
| git: add, commit, push, pull | Prompt |
| * (anything with `rm -rf`, `dd`, `> /dev/`, etc.) | Deny (via hook) |

### Load from config.toml

```toml
[permissions]
default = "prompt"         # allow | deny | prompt

[permissions.tools]
bash = "prompt"
edit = "prompt"
write = "prompt"
"git.add" = "allow"
"git.commit" = "prompt"
"git.push" = "prompt"

[permissions.glob]
"*rm -rf*" = "deny"
"*dd if=*" = "deny"
```

---

## Server ↔ Client Round-Trip for Prompt

### New ServerEvent variant

```rust
pub enum ServerEvent {
    // ... existing variants ...
    PermissionPending {
        call_id: Uuid,
        tool: String,
        subcommand: Option<String>,
        args: serde_json::Value,
        args_preview: String,   // human-readable summary
    },
    PermissionDenied {
        call_id: Uuid,
        reason: String,
    },
}
```

### New HTTP endpoint

```
POST /trees/{id}/confirm
Body: { "call_id": "uuid", "approved": true }
```

### New AgentInput variant

```rust
pub enum AgentInput {
    Message { text: String },
    Stop,
    Confirmation { call_id: Uuid, approved: bool },
}
```

### Flow

```
Agent thread              Server/HTTP              CLI client
───────────              ──────────              ──────────
1. PermissionPending
   → event_tx ─────────> broadcast to SSE ────> show [y/N] prompt
                                              │
2. User types 'y' ◄────────────────────────────┘
                        POST /confirm
3. confirm_tx ◄──────── parse, validate
   Confirmation{ok}
                        │
4. Execute tool ──────> ToolResult SSE ──────> show result
```

### Agent thread change

The agent loop acquires a `PermissionRegistry` reference. Before executing a tool call (after hooks pass), it:

1. Checks `PermissionRegistry` for the tool/subcommand/args
2. If `Allow` → execute immediately (current behavior)
3. If `Deny` → emit `PermissionDenied` event, skip execution, tell LLM it was denied
4. If `Prompt` → emit `PermissionPending` event, block on a `mpsc::Receiver<Confirmation>` waiting for a matching `call_id`

### Timeout

If no confirmation arrives within N seconds (configurable, default 120), treat as Deny and emit `PermissionDenied { reason: "timeout" }`.

---

## CLI Changes

### Interactive mode

After receiving `PermissionPending`, the CLI pauses the event-render loop and shows:

```
🛠  bash: rm -rf /important/data
  Proceed? [y/N] (timeout 120s):
```

Keypress `y`/`Y` → POST `/trees/{id}/confirm { call_id, approved: true }`
Any other key or timeout → POST `/trees/{id}/confirm { call_id, approved: false }`

### One-shot mode

Similar prompt via stderr, but if stdin is not a TTY, default to `N` (deny) or use `--yes` flag to auto-allow all prompts.

---

## Implementation Order

1. **Define types** — `PermissionLevel`, `PermissionRegistry`, new `ServerEvent` variants, `AgentInput` variant
2. **Add `PermissionRegistry` to `AgentState`** — loaded from config.toml, with sensible defaults
3. **Wire into agent loop** — add permission check between hook check and tool execution
4. **Add `/trees/{id}/confirm` route** — receives confirmation, sends through channel to agent
5. **Update CLI** — interactive and one-shot modes handle `PermissionPending` with prompt
6. **Config integration** — load permission rules from config.toml
7. **Tests** — unit tests for registry matching + integration test with temp dir

## Non-Goals

- Per-directory or per-file permissions (too complex for v1)
- Role-based access control (RBAC) — single-user tool
- Async runtime — use blocking `recv_timeout` on a channel, agent already runs in its own thread
- Changes to individual tool implementations — all logic lives in the permission check layer
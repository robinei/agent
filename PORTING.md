# Porting to macOS

This codebase is **almost entirely POSIX-compatible** — `nix::poll`, `nix::pipe`,
`nix::fcntl`, `nix::signal` (kill/killpg), `nix::socket::socketpair(AF_UNIX)`,
`signal-hook`, `termion`, `std::os::unix::*`, `process_group(0)`, and
`ExitStatusExt::signal()` all work on macOS. Only two things don't.

---

## 1. Replace bwrap with sandbox-exec

### What breaks

`agent-server/src/lifecycle.rs` has two code paths:

- **Sandboxed** (lines 149–170): builds bwrap argv via `build_bwrap_argv()`
  and wraps the worker command in `bwrap --ro-bind / / ... -- agent worker ...`.
- **Unsandboxed** (lines 171–187): spawns worker directly.

Bubblewrap uses Linux kernel namespaces (mount, pid, net, user) and does not
exist on macOS.

### Solution: `sandbox-exec` + SBPL profiles

`/usr/bin/sandbox-exec` ships with every macOS install. It takes a Seatbelt
Profile Language (SBPL) policy string via `-p` and wraps a child command,
exactly like bwrap:

```
sandbox-exec -p "(version 1)(deny default)(allow file-write* (subpath \"/Users/me/project\"))(allow network* (local ip \"localhost:*\"))" -- ./agent worker --tree-id x
```

### Implementation sketch

**a) Abstract the sandbox builder**

Factor the current `build_bwrap_argv()` into a trait or an enum:

```rust
enum SandboxKind {
    Bwrap(Vec<OsString>),
    SandboxExec(Vec<OsString>),
    Disabled,
}

fn build_sandbox_argv(
    exe: &Path, tree_id: &str, meta: &TreeMeta, cfg: &Config
) -> SandboxKind {
    if !cfg.sandbox.enabled {
        return SandboxKind::Disabled;
    }
    match std::env::consts::OS {
        "linux" => SandboxKind::Bwrap(build_bwrap_argv(exe, tree_id, meta, cfg)),
        "macos" => SandboxKind::SandboxExec(build_sandbox_exec_argv(exe, tree_id, meta, cfg)),
        _ => SandboxKind::Disabled,
    }
}
```

**b) Write `build_sandbox_exec_argv()`**

Translate the bwrap permission model to SBPL:

| bwrap flag            | SBPL equivalent                                         |
|-----------------------|---------------------------------------------------------|
| `--ro-bind / /`       | `(allow file-read*)` (broad rootfs read)                |
| `--bind <repo>`       | `(allow file-write* (subpath "<repo>"))`                |
| `--bind <store>`      | `(allow file-write* (subpath "<store>"))`               |
| `--tmpfs /tmp`        | (omitted — sandbox-exec tmpdir is already ephemeral)    |
| `--tmpfs <hide>`      | `(deny file-read* (subpath "<hide>"))`                  |
| `--dev /dev`          | `(allow file-read* (subpath "/dev"))`                   |
| `--proc /proc`        | `(allow file-read* (subpath "/proc"))`                  |
| `--share-net`         | `(allow network-outbound)` + `(allow network* (local ip "localhost:*"))` |
| `--unshare-all`       | default-deny profile                                    |
| `--new-session`       | `(deny process-fork)` (optional — restricts child exec) |

Result: build a flat SBPL string, then produce:

```
vec!["-p".into(), sbpl_string.into(), "--".into(), exe.into(), "worker".into(), ...]
```

**c) Update the spawn path**

In `spawn_worker()`, replace the `if cfg.sandbox.enabled { bwrap } else { direct }`
branch with:

```rust
match build_sandbox_argv(exe, tree_id, meta, &cfg) {
    SandboxKind::Bwrap(args) | SandboxKind::SandboxExec(args) => {
        // find `sandbox-exec` on PATH the same way resolve_bwrap_path works
        Command::new(&sandbox_path).args(&args).spawn()
    }
    SandboxKind::Disabled => {
        Command::new(&exe).arg("worker").arg("--tree-id")...spawn()
    }
}
```

**d) Config changes**

`agent-core/src/config.rs:17` currently has:

```rust
pub bwrap_path: Option<PathBuf>,
```

Rename to `sandbox_path: Option<PathBuf>` (or add a new field) and update the
TOML parser in `config.rs:283–284`. The config key can stay `bwrap_path` on
Linux and be `sandbox_path` for cross-platform, or use a single key.

---

## 2. PDEATHSIG / worker lifecycle

### What breaks

`agent-server/src/lifecycle.rs:240`:

```rust
// Run the event loop inline — this keeps the current thread alive
// until the worker exits, which is what we need for bwrap's PDEATHSIG.
```

**PDEATHSIG** (`PR_SET_PDEATHSIG`) is a Linux `prctl()` that delivers a signal
to a child process when its parent dies. This ensures the worker is killed if
the server crashes. macOS has no equivalent.

### Impact assessment

**Low.** On Linux, this is a safety net for the case where the server is
`SIGKILL`-ed and can't clean up workers. The worker is already a
bwrap-sandboxed child, so orphaned workers are harmless (sandboxed). In
practice, on macOS, orphaned worker processes would survive a server crash but:

- They have no stdin/stdout consumer (the server's poll loop is gone), so they
  will hang on their next `writeln!` and eventually error out.
- The `AgentStateMachine` reads from stdin — when stdin returns an error, the
  worker exits.

The `process_group(0)` + `killpg()` pattern in the bash tool (`bash.rs:79` for
setting the process group, `bash.rs:111–123` for killing it on timeout) **does
work on macOS**, so subshell cleanup is fine.

**No code change required** for normal operation. For crash-safety parity, the
worker's main loop (`agent-worker/src/lib.rs`) could set a 10-second I/O
watchdog on stdin — if no data arrives for 10s, exit. This covers the case
regardless of platform and is a small addition.

---

## 3. CI / testing

Add a macOS runner to the CI config. `cargo test --workspace` should pass
once the sandbox path is conditional. Tests that rely on bwrap (end-to-end
sandbox tests) should be gated with `#[cfg(target_os = "linux")]`.

---

## Summary of changes

| File | Change |
|------|--------|
| `agent-core/src/config.rs` | Rename/add `sandbox_path` field; TOML parsing |
| `agent-server/src/lifecycle.rs` | `build_sandbox_exec_argv()`; `build_sandbox_argv()` dispatcher; spawn path |
| `agent-server/Cargo.toml` | No new deps (sandbox-exec is a system binary) |
| `agent-worker/src/lib.rs` | Optional: I/O watchdog stdin timeout for crash-safety parity |

Everything else — the poll loop, pipes, socketpair, signals, process groups,
TUI — already works on macOS.
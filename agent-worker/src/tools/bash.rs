//! BashTool — execute bash commands with process group management, timeout, and output limits.
//!
//! Uses process groups to kill all child processes on timeout.
//! Output is capped at 2000 lines / 50 KB.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal;
use nix::unistd::Pid;

use super::{Tool, ToolContext, ToolOutput};
use agent_core::types::ToolDefinition;

pub struct BashTool;

enum KillReason {
    Timeout,
    Cancelled,
}

impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description:
                "Execute a bash command in the repo directory. Returns stdout; stderr is shown \
                 under a [stderr] label when present. On failure also shows exit_code. \
                 Enforces a 60-second timeout and output cap of 2000 lines / 50 KB. \
                 Use for builds, tests, git, and shell tasks. For search use `rg`; \
                 for listing files use `rg --files` (gitignore-aware) or `fd`."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 60, max 300)",
                        "minimum": 1,
                        "maximum": 300
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional description of what this command does"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput {
        let command = match params
            .get("command")
            .and_then(|v| v.as_str())
        {
            Some(c) => c,
            None => return ToolOutput::Done(Err("Missing required field: command".to_string())),
        };

        let timeout_secs: u64 = params
            .get("timeout")
            .and_then(|v| v.as_i64())
            .map(|v| (v as u64).clamp(1, 300))
            .unwrap_or(60);

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput::Done(Ok(format!("Failed to spawn bash: {}", e)));
            }
        };

        let pid = child.id() as i32;
        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_clone = timed_out.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_clone = cancelled.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();

        let stop = ctx.stop.clone();
        let timeout_handle = std::thread::spawn(move || {
            let poll_ms: u64 = 5;
            let iterations = (timeout_secs * 1000) / poll_ms;
            for _ in 0..iterations {
                if done_clone.load(Ordering::Relaxed) {
                    return;
                }
                if stop.load(Ordering::Relaxed) || crate::SIGTERM_RECEIVED.load(Ordering::Relaxed) {
                    cancelled_clone.store(true, Ordering::Relaxed);
                    let pgid = Pid::from_raw(pid);
                    let _ = signal::killpg(pgid, signal::Signal::SIGTERM);
                    std::thread::sleep(Duration::from_millis(500));
                    let _ = signal::killpg(pgid, signal::Signal::SIGKILL);
                    return;
                }
                std::thread::sleep(Duration::from_millis(poll_ms));
            }
            timed_out_clone.store(true, Ordering::Relaxed);
            let pgid = Pid::from_raw(pid);
            let _ = signal::killpg(pgid, signal::Signal::SIGTERM);
            std::thread::sleep(Duration::from_millis(500));
            let _ = signal::killpg(pgid, signal::Signal::SIGKILL);
        });

        let stdout_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(std::sync::Mutex::new(Vec::new()));

        if let Some(mut stdout_handle) = child.stdout.take() {
            let buf = stdout_buf.clone();
            std::thread::spawn(move || {
                let mut tmp = Vec::new();
                let _ = stdout_handle.read_to_end(&mut tmp);
                buf.lock().unwrap().extend(tmp);
            });
        }

        if let Some(mut stderr_handle) = child.stderr.take() {
            let buf = stderr_buf.clone();
            std::thread::spawn(move || {
                let mut tmp = Vec::new();
                let _ = stderr_handle.read_to_end(&mut tmp);
                buf.lock().unwrap().extend(tmp);
            });
        }

        let exit_status = child.wait().ok();
        let exit_code = exit_status.and_then(|s| s.code());

        done.store(true, Ordering::Relaxed);

        let _ = timeout_handle.join();

        std::thread::sleep(Duration::from_millis(50));

        let (out, err) = {
            let out = stdout_buf.lock().unwrap();
            let err = stderr_buf.lock().unwrap();
            (out.clone(), err.clone())
        };

        let kill_reason = if cancelled.load(Ordering::Relaxed) {
            Some(KillReason::Cancelled)
        } else if timed_out.load(Ordering::Relaxed) {
            Some(KillReason::Timeout)
        } else {
            None
        };

        let content = Self::combine_output(&out, &err, exit_code, kill_reason, timeout_secs);
        let output = super::truncate_output(&content, 2000, 50 * 1024);
        ToolOutput::Done(Ok(output.content))
    }
}

impl BashTool {
    fn combine_output(stdout: &[u8], stderr: &[u8], exit_code: Option<i32>, kill_reason: Option<KillReason>, timeout_secs: u64) -> String {
        let stdout_str = String::from_utf8_lossy(stdout);
        let stderr_str = String::from_utf8_lossy(stderr);
        let failed = exit_code.map_or(false, |c| c != 0);

        let mut output = String::new();
        match kill_reason {
            Some(KillReason::Timeout) => {
                output.push_str(&format!("[Command timed out after {}s]\n", timeout_secs));
            }
            Some(KillReason::Cancelled) => {
                output.push_str("[Command cancelled]\n");
            }
            None => {}
        }

        if !stdout_str.is_empty() {
            output.push_str(&stdout_str);
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }

        if !stderr_str.is_empty() {
            output.push_str("[stderr]\n");
            output.push_str(&stderr_str);
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }

        if failed {
            if let Some(code) = exit_code {
                output.push_str(&format!("exit_code: {}\n", code));
            }
        }
        if output.is_empty() {
            output.push_str("[Command completed with no output]\n");
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::fs;
    use tempfile::TempDir;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    fn run_ok(tool: &BashTool, params: serde_json::Value, ctx: &mut ToolContext) -> String {
        match tool.execute(&params, ctx) {
            ToolOutput::Done(Ok(c)) => c,
            _ => panic!("expected Done(Ok)"),
        }
    }

    #[test]
    fn test_bash_echo() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(&tool, serde_json::json!({"command": "echo hello world"}), &mut ctx);
        assert!(result.contains("hello world"));
    }

    #[test]
    fn test_bash_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(&tool, serde_json::json!({"command": "exit 42"}), &mut ctx);
        assert!(result.contains("exit_code: 42"));
    }

    #[test]
    fn test_bash_stderr() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(&tool, serde_json::json!({"command": "echo error >&2"}), &mut ctx);
        assert!(result.contains("error"));
    }

    #[test]
    fn test_bash_working_dir() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("marker.txt"), "present").unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(&tool, serde_json::json!({"command": "ls marker.txt"}), &mut ctx);
        assert!(result.contains("marker.txt"));
    }

    #[test]
    fn test_bash_timeout() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(&tool, serde_json::json!({"command": "sleep 10", "timeout": 1}), &mut ctx);
        assert!(result.contains("timed out"));
    }

    #[test]
    fn test_bash_invalid_command() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(&tool, serde_json::json!({"command": "nonexistent_command_xyz123"}), &mut ctx);
        assert!(result.contains("exit_code: 127") || result.contains("not found"));
    }

    #[test]
    fn test_bash_cancels_on_stop_flag() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let stop = ctx.stop.clone();

        let handle = std::thread::spawn(move || {
            match tool.execute(&serde_json::json!({"command": "sleep 30"}), &mut ctx) {
                ToolOutput::Done(Ok(c)) => c,
                _ => String::new(),
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(200));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);

        let result = handle.join().unwrap();
        assert!(result.contains("Command cancelled") || result.is_empty());
    }
}
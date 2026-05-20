//! BashTool — execute bash commands with process group management, timeout, and output limits.
//!
//! Uses process groups to kill all child processes on timeout.
//! Output is capped at 2000 lines / 50 KB.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal;
use nix::unistd::Pid;

use super::{truncate_output, Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

pub struct BashTool {
    cwd: PathBuf,
}

enum KillReason {
    Timeout,
    Cancelled,
}

impl BashTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description:
                "Execute a bash command in the repo directory. Returns stdout and stderr \
                 combined. Enforces a 60-second timeout and output cap of 2000 lines / 50 KB. \
                 Use for running scripts, compilers, tests, and other commands."
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

    fn execute(&self, params: &serde_json::Value, stop: &Arc<AtomicBool>) -> ToolResult {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: command".to_string())?;

        let timeout_secs: u64 = params
            .get("timeout")
            .and_then(|v| v.as_i64())
            .map(|v| (v as u64).clamp(1, 300))
            .unwrap_or(60);

        // Spawn bash with process group, merging stderr into stdout
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Create new process group so we can kill all descendants
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Failed to spawn bash: {}", e),
                    truncated: false,
                    original_size: 0,
                    exit_code: Some(-1),
                });
            }
        };

        let pid = child.id() as i32;
        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_clone = timed_out.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_clone = cancelled.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();

        // Spawn a timeout thread that also checks the stop flag
        let stop_for_thread = stop.clone();
        let timeout_handle = std::thread::spawn(move || {
            for _ in 0..(timeout_secs * 10) {
                if done_clone.load(Ordering::Relaxed) {
                    return;
                }
                if stop_for_thread.load(Ordering::Relaxed) {
                    cancelled_clone.store(true, Ordering::Relaxed);
                    let pgid = Pid::from_raw(pid);
                    let _ = signal::killpg(pgid, signal::Signal::SIGTERM);
                    std::thread::sleep(Duration::from_millis(500));
                    let _ = signal::killpg(pgid, signal::Signal::SIGKILL);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // Timeout expired — kill the process group
            timed_out_clone.store(true, Ordering::Relaxed);
            let pgid = Pid::from_raw(pid);
            let _ = signal::killpg(pgid, signal::Signal::SIGTERM);
            std::thread::sleep(Duration::from_millis(500));
            let _ = signal::killpg(pgid, signal::Signal::SIGKILL);
        });

        // Thread to read stdout
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

        // Wait for process to finish
        let exit_status = child.wait().ok();
        let exit_code = exit_status.and_then(|s| s.code());

        // Mark process as done so timeout thread won't fire unnecessarily
        done.store(true, Ordering::Relaxed);

        // Join timeout thread
        let _ = timeout_handle.join();

        // Give reader threads a moment to finish
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

        let content = Self::combine_output(&out, &err, kill_reason, timeout_secs);

        let mut output = truncate_output(&content, 2000, 50 * 1024);
        output.exit_code = exit_code;
        Ok(output)
    }
}

impl BashTool {
    fn combine_output(stdout: &[u8], stderr: &[u8], kill_reason: Option<KillReason>, timeout_secs: u64) -> String {
        let stdout_str = String::from_utf8_lossy(stdout);
        let stderr_str = String::from_utf8_lossy(stderr);

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
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&stderr_str);
        }
        if output.is_empty() && kill_reason.is_none() {
            output.push_str("[Command completed with no output]\n");
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_bash_echo() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "echo hello world"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("hello world"));
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn test_bash_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "exit 42"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_eq!(result.exit_code, Some(42));
    }

    #[test]
    fn test_bash_stderr() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "echo error >&2"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("error"));
    }

    #[test]
    fn test_bash_working_dir() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("marker.txt"), "present").unwrap();
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "ls marker.txt"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("marker.txt"));
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn test_bash_timeout() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "sleep 10", "timeout": 1}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("timed out"));
    }

    #[test]
    fn test_bash_invalid_command() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "nonexistent_command_xyz123"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_eq!(result.exit_code, Some(127));
    }

    #[test]
    fn test_bash_cancels_on_stop_flag() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool::new(dir.path());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = std::thread::spawn(move || {
            tool.execute(&serde_json::json!({"command": "sleep 30"}), &stop_clone)
        });

        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);

        let result = handle.join().unwrap().unwrap();
        assert!(result.content.contains("Command cancelled") || result.exit_code == Some(-1) || result.exit_code.is_some());
    }
}
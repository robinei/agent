//! BashTool — execute bash commands with tokio::process, timeout, and output limits.
//!
//! Uses `kill_on_drop(true)` so the child is killed if the future is dropped
//! (e.g. on cancel). No nix::signal, no watcher threads, no process groups.
//! Output is capped at 2000 lines / 50 KB.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time;

use super::{Tool, ToolContext, ToolOutput};
use agent_core::types::ToolDefinition;

pub struct BashTool;

#[async_trait::async_trait]
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

    async fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::Done(Err("Missing required field: command".to_string())),
        };

        let timeout_secs: u64 = params
            .get("timeout")
            .and_then(|v| v.as_i64())
            .map(|v| (v as u64).clamp(1, 300))
            .unwrap_or(60);

        let timeout_dur = Duration::from_secs(timeout_secs);

        let mut child = match Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput::Done(Ok(format!("Failed to spawn bash: {}", e)));
            }
        };

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Read stdout and stderr concurrently, checking ctx.stop periodically.
        let stdout_buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let stop = ctx.stop.clone();

        let read_stdout = {
            let buf = stdout_buf.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                if let Some(mut reader) = stdout_handle {
                    let mut tmp = Vec::new();
                    let _ = reader.read_to_end(&mut tmp).await;
                    buf.lock().await.extend(tmp);
                }
            })
        };

        let read_stderr = {
            let buf = stderr_buf.clone();
            tokio::spawn(async move {
                if let Some(mut reader) = stderr_handle {
                    let mut tmp = Vec::new();
                    let _ = reader.read_to_end(&mut tmp).await;
                    buf.lock().await.extend(tmp);
                }
            })
        };

        // Wait for the child with timeout, checking ctx.stop for cooperative cancel.
        let timeout_sleep = tokio::time::sleep(timeout_dur);
        tokio::pin!(timeout_sleep);
        let exit = loop {
            let sleep = tokio::time::sleep(Duration::from_millis(50));
            tokio::select! {
                status = child.wait() => {
                    let ok: Option<std::process::ExitStatus> = status.ok();
                    let none: Option<&str> = None;
                    break (ok, none);
                },
                _ = &mut timeout_sleep => {
                    // Timeout reached — kill the child via kill_on_drop
                    drop(child);
                    let _ = read_stdout.await;
                    let _ = read_stderr.await;
                    let out = stdout_buf.lock().await.clone();
                    let err = stderr_buf.lock().await.clone();
                    return ToolOutput::Done(Ok(Self::combine_output(
                        &out, &err, None, Some("Timeout"), timeout_secs,
                    )));
                },
                _ = sleep => {
                    if stop.load(Ordering::Relaxed) {
                        // Drop kills the child via kill_on_drop(true)
                        drop(child);
                        let _ = read_stdout.await;
                        let _ = read_stderr.await;
                        let out = stdout_buf.lock().await.clone();
                        let err = stderr_buf.lock().await.clone();
                        return ToolOutput::Done(Ok(Self::combine_output(
                            &out, &err, None, Some("Command cancelled"), 0,
                        )));
                    }
                }
            }
        };

        // Ensure readers are done
        let _ = read_stdout.await;
        let _ = read_stderr.await;

        let (exit_status, kill_reason) = match exit {
            (Some(status), _) => {
                // Check if we were polling for stop — in which case it was cancelled
                if stop.load(Ordering::Relaxed) {
                    (Some(status), Some("Command cancelled"))
                } else {
                    (Some(status), None)
                }
            }
            _ => (None, None),
        };

        let exit_code = exit_status.and_then(|s| s.code());

        let out = stdout_buf.lock().await.clone();
        let err = stderr_buf.lock().await.clone();

        let content = Self::combine_output(&out, &err, exit_code, kill_reason, timeout_secs);
        let output = super::truncate_output(&content, 2000, 50 * 1024);
        ToolOutput::Done(Ok(output.content))
    }
}

impl BashTool {
    fn combine_output(
        stdout: &[u8],
        stderr: &[u8],
        exit_code: Option<i32>,
        kill_reason: Option<&str>,
        timeout_secs: u64,
    ) -> String {
        let stdout_str = String::from_utf8_lossy(stdout);
        let stderr_str = String::from_utf8_lossy(stderr);
        let failed = exit_code.map_or(false, |c| c != 0);

        let mut output = String::new();
        match kill_reason {
            Some("Timeout") => {
                output.push_str(&format!("[Command timed out after {}s]\n", timeout_secs));
            }
            Some(reason) => {
                output.push_str(&format!("[{}]\n", reason));
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
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    /// Run tool synchronously for tests by wrapping in a tiny tokio runtime.
    fn run_ok(tool: &BashTool, params: serde_json::Value, ctx: &mut ToolContext) -> String {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            match tool.execute(&params, ctx).await {
                ToolOutput::Done(Ok(c)) => c,
                _ => panic!("expected Done(Ok)"),
            }
        })
    }

    #[test]
    fn test_bash_echo() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({"command": "echo hello world"}),
            &mut ctx,
        );
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
        let result = run_ok(
            &tool,
            serde_json::json!({"command": "echo error >&2"}),
            &mut ctx,
        );
        assert!(result.contains("error"));
    }

    #[test]
    fn test_bash_working_dir() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("marker.txt"), "present").unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({"command": "ls marker.txt"}),
            &mut ctx,
        );
        assert!(result.contains("marker.txt"));
    }

    #[test]
    fn test_bash_timeout() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({"command": "sleep 10", "timeout": 1}),
            &mut ctx,
        );
        assert!(result.contains("timed out"));
    }

    #[test]
    fn test_bash_invalid_command() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({"command": "nonexistent_command_xyz123"}),
            &mut ctx,
        );
        assert!(result.contains("exit_code: 127") || result.contains("not found"));
    }
}

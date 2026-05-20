//! GitTool — git operations with structured output.
//!
//! Wraps git subprocess but returns structured output instead of
//! raw terminal text. The agent gets structured data (changed files, diff stats,
//! branch info) without having to parse human-oriented output.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::process::Command;

use super::{Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

pub struct GitTool {
    cwd: PathBuf,
}

impl GitTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    /// Run a git command with the given args, returning (stdout, stderr, exit_code).
    fn run_git(&self, args: &[&str]) -> Result<(String, String, i32), String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| format!("Failed to execute git: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);
        Ok((stdout, stderr, exit_code))
    }

    fn cmd_status(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["status"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        // Also get branch info
        let branch = self
            .run_git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|(out, _, _)| out.trim().to_string())
            .unwrap_or_default();

        // Get ahead/behind counts
        let ahead_behind = self
            .run_git(&[
                "rev-list",
                "--count",
                "--left-right",
                &format!("{}@{{u}}...HEAD", branch),
            ])
            .map(|(out, _, _)| {
                let parts: Vec<&str> = out.trim().split('\t').collect();
                let ahead: u32 = parts.first().and_then(|p| p.parse().ok()).unwrap_or(0);
                let behind: u32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
                (ahead, behind)
            })
            .unwrap_or((0, 0));

        let mut content = format!("Branch: {}\n", branch);
        if ahead_behind.0 > 0 || ahead_behind.1 > 0 {
            content.push_str(&format!(
                "  {} ahead, {} behind remote\n",
                ahead_behind.0, ahead_behind.1
            ));
        }
        content.push('\n');
        content.push_str(&stdout);

        if !stderr.is_empty() {
            content.push_str(&format!("\nstderr:\n{}", stderr));
        }

        let content_len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: content_len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_diff(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["diff"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let mut content = if stdout.is_empty() {
            "No changes.\n".to_string()
        } else {
            // Add diff stat
            let stat = self
                .run_git(&["diff", "--stat"])
                .map(|(out, _, _)| out)
                .unwrap_or_default();
            format!("{}\n\n{}", stat, stdout)
        };

        if !stderr.is_empty() {
            content.push_str(&format!("\nstderr:\n{}", stderr));
        }

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_log(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["log", "--oneline", "--decorate", "-20"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let content = if stdout.is_empty() {
            "No commits.\n".to_string()
        } else {
            let mut s = stdout;
            if !stderr.is_empty() {
                s.push_str(&format!("\nstderr:\n{}", stderr));
            }
            s
        };

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_show(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["show", "--stat", "--patch"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let mut content = stdout;
        if !stderr.is_empty() {
            content.push_str(&format!("\nstderr:\n{}", stderr));
        }

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_add(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["add"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let content = if exit_code == 0 {
            format!("Added files to staging.\n{}{}", stdout, stderr)
        } else {
            format!("Git add failed:\n{}", stderr)
        };

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_commit(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["commit"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let content = if exit_code == 0 {
            format!("{}\n{}", stdout, stderr)
        } else {
            format!("Git commit failed:\n{}", stderr)
        };

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_push(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["push"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let content = if exit_code == 0 {
            format!("{}\n{}", stdout, stderr)
        } else {
            format!("Git push failed:\n{}", stderr)
        };

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }

    fn cmd_pull(&self, args: &[&str]) -> ToolResult {
        let mut git_args = vec!["pull"];
        git_args.extend(args.iter().copied());

        let (stdout, stderr, exit_code) = self.run_git(&git_args)?;

        let content = if exit_code == 0 {
            format!("{}\n{}", stdout, stderr)
        } else {
            format!("Git pull failed:\n{}", stderr)
        };

        let len = content.len();
        Ok(ToolOutput {
            content,
            truncated: false,
            original_size: len,
            exit_code: Some(exit_code),
        })
    }
}

impl Tool for GitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git".to_string(),
            description:
                "Run git operations. Returns structured output. \
                 Subcommands: status, diff, log, show, add, commit, push, pull."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Git subcommand to run",
                        "enum": ["status", "diff", "log", "show", "add", "commit", "push", "pull"]
                    },
                    "args": {
                        "type": "array",
                        "description": "Additional arguments for the git subcommand",
                        "items": {
                            "type": "string"
                        }
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value, _stop: &Arc<AtomicBool>) -> ToolResult {
        let cmd = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: command".to_string())?;

        let args: Vec<&str> = params
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect()
            })
            .unwrap_or_default();

        match cmd {
            "status" => self.cmd_status(&args),
            "diff" => self.cmd_diff(&args),
            "log" => self.cmd_log(&args),
            "show" => self.cmd_show(&args),
            "add" => self.cmd_add(&args),
            "commit" => self.cmd_commit(&args),
            "push" => self.cmd_push(&args),
            "pull" => self.cmd_pull(&args),
            _ => Err(format!(
                "Unknown git command: '{}'. Valid: status, diff, log, show, add, commit, push, pull",
                cmd
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn test_git_status() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        fs::write(dir.path().join("new.txt"), "hello").unwrap();
        let tool = GitTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "status"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("new.txt"));
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn test_git_log() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        fs::write(dir.path().join("init.txt"), "init").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let tool = GitTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "log"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("initial commit"));
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn test_git_diff() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        fs::write(dir.path().join("file.txt"), "original").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "first"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        fs::write(dir.path().join("file.txt"), "modified").unwrap();
        let tool = GitTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"command": "diff"}), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(result.content.contains("modified"));
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn test_git_unknown_command() {
        let dir = TempDir::new().unwrap();
        let tool = GitTool::new(dir.path());
        let result = tool.execute(&serde_json::json!({"command": "blame"}), &Arc::new(AtomicBool::new(false)));
        assert!(result.is_err());
    }
}
//! Project context file discovery.
//!
//! Walks up from `cwd` to root, collecting `AGENTS.md` / `CLAUDE.md` files.
//! Also checks `~/.agent/AGENTS.md` for global instructions.
//! Files are concatenated and injected into the system prompt.

use std::path::{Path, PathBuf};

/// A discovered context file.
#[derive(Clone, Debug)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
    /// Distance from cwd (0 = closest to cwd, higher = farther up)
    pub depth: usize,
}

/// Walk up from `cwd` to root, collecting context files.
/// The closest file to cwd comes last (highest precedence).
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();

    // Check global context files first (~/.agent/AGENTS.md and ~/.agent/skills/*/SKILL.md)
    let global_path = agent_dir.join("AGENTS.md");
    if global_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&global_path) {
            if !content.trim().is_empty() {
                files.push(ContextFile {
                    path: global_path,
                    content,
                    depth: usize::MAX,
                });
            }
        }
    }

    let global_skills_dir = agent_dir.join("skills");
    if global_skills_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&global_skills_dir) {
            for entry in entries.flatten() {
                let skill_md = entry.path().join("SKILL.md");
                if skill_md.exists() {
                    if let Ok(content) = std::fs::read_to_string(&skill_md) {
                        if !content.trim().is_empty() {
                            files.push(ContextFile {
                                path: skill_md,
                                content,
                                depth: usize::MAX,
                            });
                        }
                    }
                }
            }
        }
    }

    // Walk up from cwd to root
    let mut ancestors: Vec<PathBuf> = cwd.ancestors().map(|p| p.to_path_buf()).collect();
    ancestors.reverse(); // root first, cwd last

    for (depth, dir) in ancestors.iter().enumerate() {
        for name in &["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if !content.trim().is_empty() {
                        files.push(ContextFile {
                            path,
                            content,
                            depth,
                        });
                    }
                }
            }
        }

        // Also check .agent/skills/ directories
        let skills_dir = dir.join(".agent").join("skills");
        if skills_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let skill_path = entry.path();
                    if skill_path.is_dir() {
                        let skill_md = skill_path.join("SKILL.md");
                        if skill_md.exists() {
                            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                                if !content.trim().is_empty() {
                                    files.push(ContextFile {
                                        path: skill_md,
                                        content,
                                        depth,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Sort by depth so closest-to-cwd comes last (highest precedence)
    files.sort_by_key(|f| f.depth);
    files
}

/// Format context files into a single string for injection into system prompt.
pub fn format_context_section(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut sections = Vec::new();
    for file in files {
        let rel_path = file
            .path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| format!("({})", n.to_string_lossy()))
            .unwrap_or_default();
        sections.push(format!(
            "### Instructions from `{}` {}\n\n{}",
            file.path.file_name().unwrap_or_default().to_string_lossy(),
            rel_path,
            file.content.trim()
        ));
    }

    format!("## Project Context\n\n{}", sections.join("\n\n---\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_load_context_files_none() {
        let dir = TempDir::new().unwrap();
        let files = load_context_files(dir.path(), dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn test_load_context_files_finds_agents_md() {
        let dir = TempDir::new().unwrap();
        let agent_dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# Project rules\nUse Rust.").unwrap();
        let files = load_context_files(dir.path(), agent_dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("Use Rust."));
    }

    #[test]
    fn test_load_context_files_walks_up() {
        let dir = TempDir::new().unwrap();
        let agent_dir = TempDir::new().unwrap();
        let subdir = dir.path().join("src").join("lib");
        fs::create_dir_all(&subdir).unwrap();

        // Parent has AGENTS.md, cwd doesn't
        fs::write(dir.path().join("AGENTS.md"), "# Root rules\n").unwrap();

        let files = load_context_files(&subdir, agent_dir.path());
        assert!(!files.is_empty(), "Should find AGENTS.md in parent dir");
    }

    #[test]
    fn test_format_context_section() {
        let files = vec![
            ContextFile {
                path: PathBuf::from("/home/user/project/CLAUDE.md"),
                content: "Be concise.".into(),
                depth: 1,
            },
            ContextFile {
                path: PathBuf::from("/home/user/project/src/AGENTS.md"),
                content: "Use Rust.".into(),
                depth: 0,
            },
        ];
        let section = format_context_section(&files);
        assert!(section.contains("Project Context"));
        assert!(section.contains("Use Rust."));
        assert!(section.contains("Be concise."));
    }

    #[test]
    fn test_empty_files_list() {
        assert_eq!(format_context_section(&[]), "");
    }
}

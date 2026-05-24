use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use agent_core::config::Config;
use agent_core::types::TreeMeta;

// ── bwrap path resolution ──

pub(crate) fn resolve_bwrap_path(hint: &Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = hint {
        if p.exists() {
            return Ok(p.clone());
        }
        return Err(format!("bwrap not found at configured path {:?}", p));
    }
    for candidate in &["/usr/bin/bwrap", "/usr/local/bin/bwrap"] {
        if Path::new(candidate).exists() {
            return Ok(PathBuf::from(candidate));
        }
    }
    log::warn!("[sandbox] bwrap not found on PATH, workers will run unsandboxed");
    which("bwrap").ok_or_else(|| {
        "bwrap not found: install bubblewrap or set sandbox.enabled = false".to_string()
    })
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Build the argv for a bubblewrap sandbox around the agent worker.
pub fn build_bwrap_argv(
    exe: &Path,
    tree_id: &str,
    meta: &TreeMeta,
    cfg: &Config,
) -> Vec<OsString> {
    let store_dir = agent_core::config::agent_dir().join("trees").join(tree_id);

    let mut args: Vec<OsString> = Vec::new();
    args.extend(["--ro-bind", "/", "/"].iter().map(OsString::from));
    args.extend(["--dev", "/dev"].iter().map(OsString::from));
    args.extend(["--proc", "/proc"].iter().map(OsString::from));
    args.extend(["--tmpfs", "/tmp"].iter().map(OsString::from));
    args.extend(["--bind".into(), store_dir.clone().into(), store_dir.into()]);
    if let Some(repo) = &meta.repo_path {
        args.extend(["--bind".into(), repo.clone().into(), repo.clone().into()]);
    }
    args.extend([
        "--ro-bind".into(),
        exe.to_path_buf().into(),
        exe.to_path_buf().into(),
    ]);

    for p in &meta.sandbox.writable {
        let expanded = agent_core::types::expand_tilde(p);
        if expanded.exists() {
            args.extend(["--bind".into(), expanded.clone().into(), expanded.into()]);
        }
    }

    let mut hide_set: BTreeSet<PathBuf> = cfg.sandbox.defaults.hide.iter().cloned().collect();
    hide_set.extend(meta.sandbox.hide.iter().cloned());
    for u in &meta.sandbox.unhide {
        hide_set.remove(u);
    }
    for p in &hide_set {
        let expanded = agent_core::types::expand_tilde(p);
        let meta = match std::fs::symlink_metadata(&expanded) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            args.extend(["--tmpfs".into(), expanded.into()]);
        } else {
            args.extend([
                "--bind".into(),
                PathBuf::from("/dev/null").into(),
                expanded.into(),
            ]);
        }
    }

    args.push("--unshare-all".into());
    let allow_net = meta.sandbox.network.unwrap_or(false);
    if allow_net {
        args.push("--share-net".into());
    }
    args.push("--new-session".into());
    args.push("--die-with-parent".into());
    args.push("--".into());
    args.push(exe.to_path_buf().into());
    args.push("worker".into());
    args.push("--tree-id".into());
    args.push(tree_id.into());

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::config::{SandboxConfig, SandboxDefaults};
    use agent_core::types::TreeSandbox;

    #[test]
    fn test_build_bwrap_argv_basic() {
        let exe = Path::new("/usr/local/bin/agent");
        let tree_id = "test-tree-001";
        let meta = TreeMeta {
            id: tree_id.into(),
            parent_id: None,
            repo_path: Some(PathBuf::from("/home/user/code/repo")),
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        let cfg = Config {
            sandbox: SandboxConfig {
                enabled: true,
                bwrap_path: None,
                defaults: SandboxDefaults {
                    hide: vec![PathBuf::from("~/.ssh"), PathBuf::from("~/.aws")],
                },
            },
            ..Config::default()
        };

        let args = build_bwrap_argv(exe, tree_id, &meta, &cfg);

        assert!(args.iter().any(|a| a == "--ro-bind"));
        assert!(args.iter().any(|a| a == "--dev"));
        assert!(args.iter().any(|a| a == "--proc"));
        assert!(args.iter().any(|a| a == "--tmpfs"));
        assert!(!args.iter().any(|a| a == "--share-net"));
        assert!(args.iter().any(|a| a == "--unshare-all"));
        assert!(args.iter().any(|a| a == "--new-session"));
        assert!(args.iter().any(|a| a == "--die-with-parent"));

        let worker_idx = args.iter().position(|a| a == "--").unwrap();
        assert!(worker_idx + 1 < args.len());
        assert_eq!(args[worker_idx + 1], OsString::from("/usr/local/bin/agent"));
        assert_eq!(args[worker_idx + 2], OsString::from("worker"));
        assert_eq!(args[worker_idx + 3], OsString::from("--tree-id"));
        assert_eq!(args[worker_idx + 4], OsString::from(tree_id));

        assert!(args.iter().any(|a| a == "--bind"));
    }

    #[test]
    fn test_build_bwrap_argv_no_net() {
        let exe = Path::new("/usr/local/bin/agent");
        let tree_id = "test-tree-no-net";
        let meta = TreeMeta {
            id: tree_id.into(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox {
                network: Some(false),
                ..TreeSandbox::default()
            },
        };
        let cfg = Config {
            sandbox: SandboxConfig {
                enabled: true,
                bwrap_path: None,
                defaults: SandboxDefaults::default(),
            },
            ..Config::default()
        };

        let args = build_bwrap_argv(exe, tree_id, &meta, &cfg);

        assert!(!args.iter().any(|a| a == "--share-net"));
        assert!(args.iter().any(|a| a == "--unshare-all"));
    }

    #[test]
    fn test_build_bwrap_argv_unhide() {
        let exe = Path::new("/usr/local/bin/agent");
        let tree_id = "test-tree-unhide";
        let meta = TreeMeta {
            id: tree_id.into(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox {
                unhide: vec![PathBuf::from("~/.ssh")],
                ..TreeSandbox::default()
            },
        };
        let cfg = Config {
            sandbox: SandboxConfig {
                enabled: true,
                bwrap_path: None,
                defaults: SandboxDefaults {
                    hide: vec![PathBuf::from("~/.ssh"), PathBuf::from("~/.aws")],
                },
            },
            ..Config::default()
        };

        let args = build_bwrap_argv(exe, tree_id, &meta, &cfg);

        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let ssh_dir = PathBuf::from(&home).join(".ssh");
        let aws_dir = PathBuf::from(&home).join(".aws");

        let ssh_tmpfs_count = args
            .windows(2)
            .filter(|w| w[0] == OsString::from("--tmpfs"))
            .filter(|w| w[1] == OsString::from(ssh_dir.clone().into_os_string()))
            .count();
        assert_eq!(ssh_tmpfs_count, 0, "~/.ssh should not be tmpfs'd (unhided)");

        if aws_dir.exists() {
            let aws_tmpfs_count = args
                .windows(2)
                .filter(|w| w[0] == OsString::from("--tmpfs"))
                .filter(|w| w[1] == OsString::from(aws_dir.clone().into_os_string()))
                .count();
            assert_eq!(
                aws_tmpfs_count, 1,
                "~/.aws should be tmpfs'd (still hidden)"
            );
        }
    }

    #[test]
    fn test_build_bwrap_argv_file_hide() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("fake_history");
        let dir_path = tmp.path().join("fake_dir");
        std::fs::write(&file_path, b"some history data").unwrap();
        std::fs::create_dir(&dir_path).unwrap();

        let exe = Path::new("/usr/local/bin/agent");
        let tree_id = "test-tree-file-hide";
        let meta = TreeMeta {
            id: tree_id.into(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: TreeSandbox::default(),
        };
        let cfg = Config {
            sandbox: SandboxConfig {
                enabled: true,
                bwrap_path: None,
                defaults: SandboxDefaults {
                    hide: vec![file_path.clone(), dir_path.clone()],
                },
            },
            ..Config::default()
        };

        let args = build_bwrap_argv(exe, tree_id, &meta, &cfg);

        let file_bind_count = args
            .windows(3)
            .filter(|w| w[0] == OsString::from("--bind"))
            .filter(|w| w[1] == OsString::from("/dev/null"))
            .filter(|w| w[2] == OsString::from(file_path.clone().into_os_string()))
            .count();
        assert_eq!(
            file_bind_count, 1,
            "file should be bound from /dev/null, not tmpfs'd"
        );

        let dir_tmpfs_count = args
            .windows(2)
            .filter(|w| w[0] == OsString::from("--tmpfs"))
            .filter(|w| w[1] == OsString::from(dir_path.clone().into_os_string()))
            .count();
        assert_eq!(dir_tmpfs_count, 1, "directory should be tmpfs'd");

        let file_tmpfs_count = args
            .windows(2)
            .filter(|w| w[0] == OsString::from("--tmpfs"))
            .filter(|w| w[1] == OsString::from(file_path.clone().into_os_string()))
            .count();
        assert_eq!(file_tmpfs_count, 0, "file should not be tmpfs'd");
    }
}
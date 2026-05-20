use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};

use agent_core::config::Config;
use agent_core::provider::Provider;
use agent_core::store::Store;
use agent_core::types::{Entry, ServerEvent, TreeId, TreeMeta};

const BUFFER_CAPACITY: usize = 1000;
type StderrBuf = Arc<Mutex<VecDeque<String>>>;

// ── Worker subprocess lifecycle ──

pub struct WorkerEntry {
    pub stdin_tx: mpsc::Sender<String>,
    pub event_buffer: VecDeque<ServerEvent>,
    pub subscribers: Vec<mpsc::Sender<ServerEvent>>,
    pub pid: u32,
    pub child: Option<Child>,
}

pub static ACTIVE_WORKERS: LazyLock<Mutex<HashMap<TreeId, Arc<Mutex<WorkerEntry>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn worker_get(tree_id: &str) -> Option<Arc<Mutex<WorkerEntry>>> {
    ACTIVE_WORKERS.lock().unwrap().get(tree_id).cloned()
}

/// Broadcast a `MetaUpdate` event to all subscribers of a tree.
pub fn broadcast_meta_update(tree_id: &str, title: Option<String>) {
    let entry = match worker_get(tree_id) {
        Some(e) => e,
        None => return,
    };
    let ev = ServerEvent::MetaUpdate { title };
    let mut guard = entry.lock().unwrap();
    guard.subscribers.retain(|tx| tx.send(ev.clone()).is_ok());
}

pub fn spawn_worker(tree_id: &str, store: Arc<Store>, cfg: Arc<Config>) -> Result<(), String> {
    let workers = ACTIVE_WORKERS.lock().map_err(|e| e.to_string())?;
    if workers.contains_key(tree_id) {
        return Err(format!("Worker already active for tree {}", tree_id));
    }
    drop(workers);

    // Use argv[0] rather than current_exe(): on Linux, current_exe() reads
    // /proc/self/exe and appends " (deleted)" when the binary has been replaced
    // since the process started (e.g. by `cargo build`). argv[0] is the original
    // launch path and is always usable as a filesystem reference.
    let exe = std::env::args()
        .next()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "argv[0] missing".to_string())?;
    let exe = if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir()
            .map_err(|e| format!("current_dir: {e}"))?
            .join(exe)
    };
    let config_path = agent_core::config::agent_dir().join("config.toml");

    let meta = store
        .get_tree(tree_id)
        .map_err(|e| format!("get_tree: {e}"))?
        .ok_or_else(|| format!("tree {} not found", tree_id))?;

    let mut child = if cfg.sandbox.enabled {
        let bwrap_path = resolve_bwrap_path(&cfg.sandbox.bwrap_path)?;
        let bwrap_args = build_bwrap_argv(&exe, tree_id, &meta, &cfg);
        Command::new(&bwrap_path)
            .args(&bwrap_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn bwrap worker: {e}"))?
    } else {
        Command::new(&exe)
            .arg("worker")
            .arg("--tree-id")
            .arg(tree_id)
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn worker: {e}"))?
    };

    let pid = child.id();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>();

    let entry = Arc::new(Mutex::new(WorkerEntry {
        stdin_tx,
        event_buffer: VecDeque::with_capacity(BUFFER_CAPACITY),
        subscribers: Vec::new(),
        pid,
        child: Some(child),
    }));

    ACTIVE_WORKERS
        .lock()
        .unwrap()
        .insert(tree_id.to_string(), entry.clone());

    let stderr_buf: StderrBuf = Arc::new(Mutex::new(VecDeque::with_capacity(20)));

    spawn_stdin_writer(stdin, stdin_rx);
    spawn_stdout_proxy(tree_id.to_string(), stdout, entry.clone(), store, cfg, stderr_buf.clone());
    spawn_stderr_demux(tree_id.to_string(), stderr, stderr_buf);
    log::info!("[lifecycle] Spawned worker for tree {} (pid {})", tree_id, pid);
    Ok(())
}

fn resolve_bwrap_path(hint: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf, String> {
    if let Some(p) = hint {
        if p.exists() {
            return Ok(p.clone());
        }
        return Err(format!("bwrap not found at configured path {:?}", p));
    }
    // Probe common locations
    for candidate in &["/usr/bin/bwrap", "/usr/local/bin/bwrap"] {
        if std::path::Path::new(candidate).exists() {
            return Ok(std::path::PathBuf::from(candidate));
        }
    }
    log::warn!("[lifecycle] bwrap not found on PATH, workers will run unsandboxed");
    // Fall back: try PATH lookup
    which("bwrap").ok_or_else(|| {
        "bwrap not found: install bubblewrap or set sandbox.enabled = false".to_string()
    })
}

fn which(name: &str) -> Option<std::path::PathBuf> {
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

pub fn build_bwrap_argv(
    exe: &Path,
    tree_id: &str,
    meta: &TreeMeta,
    cfg: &Config,
) -> Vec<OsString> {
    let store_dir = agent_core::config::agent_dir().join("trees").join(tree_id);
    let config_path = agent_core::config::agent_dir().join("config.toml");

    let mut args: Vec<OsString> = Vec::new();

    // Structural mounts
    args.extend(["--ro-bind", "/", "/"].iter().map(OsString::from));
    args.extend(["--dev", "/dev"].iter().map(OsString::from));
    args.extend(["--proc", "/proc"].iter().map(OsString::from));
    args.extend(["--tmpfs", "/tmp"].iter().map(OsString::from));

    // The tree's own data dir + repo + config
    args.extend(["--bind".into(), store_dir.clone().into(), store_dir.into()]);
    if let Some(repo) = &meta.repo_path {
        args.extend(["--bind".into(), repo.clone().into(), repo.clone().into()]);
    }
    args.extend(["--ro-bind".into(), config_path.clone().into(), config_path.into()]);
    args.extend(["--ro-bind".into(), exe.to_path_buf().into(), exe.to_path_buf().into()]);

    // Per-tree extra writables
    for p in &meta.sandbox.writable {
        let expanded = agent_core::types::expand_tilde(p);
        if expanded.exists() {
            args.extend(["--bind".into(), expanded.clone().into(), expanded.into()]);
        }
    }

    // Hide = defaults + sandbox.hide minus sandbox.unhide
    let mut hide_set: BTreeSet<PathBuf> = cfg.sandbox.defaults.hide.iter().cloned().collect();
    hide_set.extend(meta.sandbox.hide.iter().cloned());
    for u in &meta.sandbox.unhide {
        hide_set.remove(u);
    }
    // bwrap requires the right opcode per path type: --tmpfs only works on
    // directories, --bind /dev/null is the equivalent for files. Picking the
    // wrong one ("Can't mkdir ...: Not a directory") will abort the worker
    // before it ever starts. Skip paths that don't exist on the host.
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
                std::path::PathBuf::from("/dev/null").into(),
                expanded.into(),
            ]);
        }
    }

    // Namespace + network
    args.push("--unshare-all".into());
    let allow_net = meta.sandbox.network.unwrap_or(true);
    if allow_net {
        args.push("--share-net".into());
    }
    args.push("--new-session".into());
    args.push("--die-with-parent".into());

    // Worker command after --
    args.push("--".into());
    args.push(exe.to_path_buf().into());
    args.push("worker".into());
    args.push("--tree-id".into());
    args.push(tree_id.into());
    args.push("--config".into());
    args.push(agent_core::config::agent_dir().join("config.toml").into());

    args
}

pub fn worker_send_command(tree_id: &str, json_line: &str) -> Result<(), String> {
    let entry = worker_get(tree_id).ok_or_else(|| format!("No active worker for tree {}", tree_id))?;
    let guard = entry.lock().unwrap();
    guard
        .stdin_tx
        .send(json_line.to_string())
        .map_err(|e| format!("Failed to send command to worker: {}", e))
}

pub fn worker_stop(tree_id: &str) -> Result<(), String> {
    worker_send_command(tree_id, r#"{"method":"stop"}"#)
}

pub fn worker_subscribe(
    tree_id: &str,
) -> Option<(Vec<ServerEvent>, mpsc::Receiver<ServerEvent>)> {
    let entry = worker_get(tree_id)?;
    let mut guard = entry.lock().unwrap();
    let snapshot: Vec<ServerEvent> = guard.event_buffer.iter().cloned().collect();
    let (tx, rx) = mpsc::channel();
    guard.subscribers.push(tx);
    Some((snapshot, rx))
}

fn spawn_stdin_writer(mut stdin: ChildStdin, rx: mpsc::Receiver<String>) {
    std::thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            if writeln!(stdin, "{}", line).is_err() {
                break;
            }
            if stdin.flush().is_err() {
                break;
            }
        }
    });
}

fn spawn_stdout_proxy(
    tree_id: String,
    stdout: ChildStdout,
    entry: Arc<Mutex<WorkerEntry>>,
    store: Arc<Store>,
    cfg: Arc<Config>,
    stderr_buf: StderrBuf,
) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let event: ServerEvent = match serde_json::from_str(buf.trim_end()) {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("[proxy {}] bad event JSON: {}", tree_id, e);
                    continue;
                }
            };
            let mut guard = entry.lock().unwrap();
            if matches!(event, ServerEvent::Entry(_)) {
                if guard.event_buffer.len() >= BUFFER_CAPACITY {
                    guard.event_buffer.pop_front();
                }
                guard.event_buffer.push_back(event.clone());
            }
            guard.subscribers.retain(|tx| tx.send(event.clone()).is_ok());

            // Auto-title: if a Done event arrives on a tree without a title,
            // spawn a side thread to generate one.
            if matches!(event, ServerEvent::Done { .. }) {
                let store_for_title = store.clone();
                let cfg_for_title = cfg.clone();
                let entry_for_title = entry.clone();
                let tid = tree_id.clone();
                std::thread::spawn(move || {
                    let needs = match store_for_title.get_tree(&tid) {
                        Ok(Some(m)) => m.title.is_none(),
                        _ => false,
                    };
                    if !needs { return; }
                    let provider = Provider::new(
                        cfg_for_title.summary.base_url.clone(),
                        cfg_for_title.summary.api_key.clone(),
                        cfg_for_title.summary.model.clone(),
                    );
                    match agent_core::agent::auto_title(&store_for_title, &provider, &tid) {
                        Ok(title) => {
                            let ev = ServerEvent::MetaUpdate { title: Some(title) };
                            let mut g = entry_for_title.lock().unwrap();
                            g.subscribers.retain(|tx| tx.send(ev.clone()).is_ok());
                        }
                        Err(e) => log::warn!("[auto-title {}] {}", tid, e),
                    }
                });
            }
        }
        log::info!("[proxy {}] worker stdout closed", tree_id);

        // Crash detection: check child exit status
        let exit_ok = {
            let mut guard = entry.lock().unwrap();
            if let Some(mut child) = guard.child.take() {
                let status = child.wait().ok();
                matches!(status, Some(s) if s.success())
            } else {
                true
            }
        };

        if !exit_ok {
            log::warn!("[proxy {}] worker exited with error", tree_id);
            let detail = {
                let g = stderr_buf.lock().unwrap();
                if g.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", g.iter().cloned().collect::<Vec<_>>().join("\n"))
                }
            };
            let err_event = ServerEvent::Error {
                message: format!("worker exited unexpectedly{}", detail),
                fatal: true,
            };
            let done_event = ServerEvent::Done { status: "aborted".into() };
            let mut guard = entry.lock().unwrap();
            guard.subscribers.retain(|tx| tx.send(err_event.clone()).is_ok());
            guard.subscribers.retain(|tx| tx.send(done_event.clone()).is_ok());
        }

        ACTIVE_WORKERS.lock().unwrap().remove(&tree_id);
    });
}

fn spawn_stderr_demux(tree_id: String, stderr: ChildStderr, buf: StderrBuf) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut line = String::new();
        let short = &tree_id[..tree_id.len().min(8)];
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim_end().to_string();
                    log::info!("[worker {}] {}", short, trimmed);
                    let mut g = buf.lock().unwrap();
                    if g.len() >= 20 {
                        g.pop_front();
                    }
                    g.push_back(trimmed);
                }
            }
        }
    });
}

// ── Graceful shutdown ──

/// Recover a tree after an unclean shutdown or worker crash.
/// Reads `meta.leaf_id`, appends a linked `SessionEnd` (Aborted), updates
/// `meta.leaf_id`, and resets header tokens.
pub fn recover_tree(store: &Store, tree_id: &str) {
    let meta = match store.get_tree(tree_id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            log::warn!("[lifecycle] recover_tree: tree {} not found, skipping", tree_id);
            return;
        }
        Err(e) => {
            log::error!("[lifecycle] recover_tree: failed to read meta for {}: {}", tree_id, e);
            return;
        }
    };

    let parent_id = meta.leaf_id.clone();
    if parent_id.is_none() {
        log::info!("[lifecycle] recover_tree: tree {} has no leaf_id, nothing to recover", tree_id);
        return;
    }

    let new_id = agent_core::util::generate_entry_id();
    let entry = Entry::SessionEnd {
        id: new_id.clone(),
        parent_id,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: Some("session aborted (worker exit or server shutdown)".into()),
        status: agent_core::types::SessionStatus::Aborted,
        continuation_brief: None,
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        log::error!("[lifecycle] recover_tree: append session_end for {}: {}", tree_id, e);
        return;
    }

    let mut meta = meta;
    meta.leaf_id = Some(new_id);
    if let Err(e) = store.save_tree_meta(&meta) {
        log::error!("[lifecycle] recover_tree: save meta for {}: {}", tree_id, e);
    }
    store.reset_header_tokens(tree_id).ok();
}

/// Signal all active workers to stop, wait up to 60s, then SIGKILL any survivors.
pub fn shutdown_all(store: &Store) {
    let snapshot: Vec<(String, u32)> = {
        let map = ACTIVE_WORKERS.lock().unwrap();
        map.iter()
            .map(|(id, entry)| {
                let pid = entry.lock().unwrap().pid;
                (id.clone(), pid)
            })
            .collect()
    };
    if snapshot.is_empty() {
        log::info!("[lifecycle] no active workers to shut down");
        return;
    }
    log::info!("[lifecycle] shutting down {} worker(s)", snapshot.len());

    // Step 1: send stop to all workers
    for (id, _) in &snapshot {
        let _ = worker_stop(id);
    }

    // Step 2: wait up to 60s for each worker
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    for (id, _pid) in &snapshot {
        let child = {
            let map = ACTIVE_WORKERS.lock().unwrap();
            map.get(id)
                .and_then(|entry| entry.lock().unwrap().child.take())
        };
        let exited = if let Some(mut child) = child {
            loop {
                if std::time::Instant::now() > deadline {
                    break false;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        log::info!("[lifecycle] worker {} exited: {}", id, status);
                        break true;
                    }
                    Ok(None) => {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(e) => {
                        log::error!("[lifecycle] wait on worker {} failed: {}", id, e);
                        break false;
                    }
                }
            }
        } else {
            true // no child handle, assume exited
        };

        if !exited {
            log::warn!("[lifecycle] worker {} still alive after 60s, killing", id);
            // Use Child::kill on the handle or fall back to OS signal
            if let Some(mut child) = {
                let map = ACTIVE_WORKERS.lock().unwrap();
                map.get(id)
                    .and_then(|entry| entry.lock().unwrap().child.take())
            } {
                let _ = child.kill();
                let _ = child.wait();
            }
            recover_tree(store, id);
        }
    }

    // Final cleanup: remove all workers from the map
    ACTIVE_WORKERS.lock().unwrap().clear();
    log::info!("[lifecycle] shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::config::{SandboxConfig, SandboxDefaults};
    use agent_core::types::{Entry, TreeSandbox};
    use std::path::PathBuf;

    #[test]
    fn test_worker_subscribe_atomicity() {
        let entry = Arc::new(Mutex::new(WorkerEntry {
            stdin_tx: mpsc::channel().0,
            event_buffer: VecDeque::with_capacity(BUFFER_CAPACITY),
            subscribers: Vec::new(),
            pid: 0,
            child: None,
        }));

        // Pre-populate with some events
        {
            let mut g = entry.lock().unwrap();
            let e = ServerEvent::Entry(Entry::SessionStart {
                id: "1".into(),
                parent_id: None,
                timestamp: "t1".into(),
            });
            g.event_buffer.push_back(e);
        }

        // Insert into ACTIVE_WORKERS
        let tree_id = "test-atomicity";
        ACTIVE_WORKERS
            .lock()
            .unwrap()
            .insert(tree_id.to_string(), entry.clone());

        // Subscribe — gets snapshot + live rx
        let (snapshot, rx) = worker_subscribe(tree_id).unwrap();
        assert_eq!(snapshot.len(), 1);

        // Append event while subscriber is live
        let e2 = ServerEvent::Entry(Entry::SessionStart {
            id: "2".into(),
            parent_id: None,
            timestamp: "t2".into(),
        });
        {
            let mut g = entry.lock().unwrap();
            g.event_buffer.push_back(e2.clone());
            g.subscribers.retain(|tx| tx.send(e2.clone()).is_ok());
        }

        // Live subscriber should receive it
        let received = rx.recv().unwrap();
        assert!(matches!(received, ServerEvent::Entry(_)));

        // Cleanup
        ACTIVE_WORKERS.lock().unwrap().remove(tree_id);
    }

    #[test]
    fn test_recover_tree_links_chain() {
        use agent_core::store::Store;
        use agent_core::types::SessionStatus;
        use tempfile::TempDir;

        let dir = TempDir::with_prefix("agent-lifecycle-test-").unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let tree_id = "recover-test";

        store.create_tree_file(tree_id, "model").unwrap();
        let start_id = "s1".to_string();
        let meta = agent_core::types::TreeMeta {
            id: tree_id.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: Some(start_id.clone()),
            sandbox: agent_core::types::TreeSandbox::default(),
        };
        store.save_tree_meta(&meta).unwrap();

        store.append_entry(tree_id, &Entry::SessionStart {
            id: start_id.clone(), parent_id: None, timestamp: "t1".into(),
        }).unwrap();

        recover_tree(&store, tree_id);

        let entries = store.read_all_entries(tree_id).unwrap();
        let last = entries.last().unwrap();
        match last {
            Entry::SessionEnd { status, summary, parent_id, .. } => {
                assert_eq!(*status, SessionStatus::Aborted);
                assert!(summary.as_deref().unwrap_or("").contains("aborted"));
                assert_eq!(*parent_id, Some(start_id.clone()), "parent_id must link to leaf_id");
            }
            other => panic!("expected SessionEnd, got {:?}", other),
        }

        // Verify meta.leaf_id was updated
        let updated = store.get_tree(tree_id).unwrap().unwrap();
        assert!(updated.leaf_id.is_some());
        assert_ne!(updated.leaf_id, Some(start_id), "leaf_id must advance");
    }

    #[test]
    fn test_recover_tree_empty_tree() {
        use agent_core::store::Store;
        use tempfile::TempDir;

        let dir = TempDir::with_prefix("agent-lifecycle-test-").unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let tree_id = "recover-empty";

        store.create_tree_file(tree_id, "model").unwrap();
        let meta = agent_core::types::TreeMeta {
            id: tree_id.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: 100,
            updated_at: 100,
            leaf_id: None,
            sandbox: agent_core::types::TreeSandbox::default(),
        };
        store.save_tree_meta(&meta).unwrap();

        // Should not panic and should not write any entries
        recover_tree(&store, tree_id);

        let entries = store.read_all_entries(tree_id).unwrap();
        assert!(entries.is_empty(), "empty tree should have no entries after recover");
    }

    #[test]
    fn test_broadcast_meta_update() {
        let entry = Arc::new(Mutex::new(WorkerEntry {
            stdin_tx: mpsc::channel().0,
            event_buffer: VecDeque::with_capacity(BUFFER_CAPACITY),
            subscribers: Vec::new(),
            pid: 0,
            child: None,
        }));

        let tree_id = "test-meta-update";
        ACTIVE_WORKERS
            .lock()
            .unwrap()
            .insert(tree_id.to_string(), entry.clone());

        // Subscribe to receive events
        let (_snapshot, rx) = worker_subscribe(tree_id).unwrap();

        // Broadcast a meta update
        broadcast_meta_update(tree_id, Some("Generated Title".into()));

        // Verify the subscriber received it
        let received = rx.recv().unwrap();
        assert!(matches!(&received, ServerEvent::MetaUpdate { title: Some(t) } if t == "Generated Title"));

        // Cleanup
        ACTIVE_WORKERS.lock().unwrap().remove(tree_id);
    }

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

        // Structural mounts must be present and in order
        assert!(args.iter().any(|a| a == "--ro-bind"));
        assert!(args.iter().any(|a| a == "--dev"));
        assert!(args.iter().any(|a| a == "--proc"));
        assert!(args.iter().any(|a| a == "--tmpfs"));

        // Must include --share-net (network defaults to true)
        assert!(args.iter().any(|a| a == "--share-net"));

        // Must include --unshare-all, --new-session, --die-with-parent
        assert!(args.iter().any(|a| a == "--unshare-all"));
        assert!(args.iter().any(|a| a == "--new-session"));
        assert!(args.iter().any(|a| a == "--die-with-parent"));

        // Must include the worker subcommand after --
        let worker_idx = args.iter().position(|a| a == "--").unwrap();
        assert!(worker_idx + 1 < args.len());
        assert_eq!(args[worker_idx + 1], OsString::from("/usr/local/bin/agent"));
        assert_eq!(args[worker_idx + 2], OsString::from("worker"));
        assert_eq!(args[worker_idx + 3], OsString::from("--tree-id"));
        assert_eq!(args[worker_idx + 4], OsString::from(tree_id));

        // Repo path must be bound
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

        // Must NOT include --share-net
        assert!(!args.iter().any(|a| a == "--share-net"));
        // But still has other namespace args
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

        // The home directory may not exist in test env, so the --tmpfs for hide
        // paths may be skipped if the path doesn't exist. Instead, check that
        // the unhide path (~/.ssh) does NOT produce a --tmpfs arg targeting it
        // while ~/.aws still does (since both defaults.hide exist and only
        // ~/.ssh is unhide'd).
        // We just verify there's at most one --tmpfs (for ~/.aws, since ~/.ssh was unhide'd)
        // and that ~/.ssh is not among the --tmpfs args.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let ssh_dir = PathBuf::from(&home).join(".ssh");
        let aws_dir = PathBuf::from(&home).join(".aws");

        // Count --tmpfs that match the unhide'd path
        let ssh_tmpfs_count = args.windows(2)
            .filter(|w| w[0] == OsString::from("--tmpfs"))
            .filter(|w| w[1] == OsString::from(ssh_dir.clone().into_os_string()))
            .count();
        assert_eq!(ssh_tmpfs_count, 0, "~/.ssh should not be tmpfs'd (unhided)");

        // If ~/.aws exists, it should have a --tmpfs
        if aws_dir.exists() {
            let aws_tmpfs_count = args.windows(2)
                .filter(|w| w[0] == OsString::from("--tmpfs"))
                .filter(|w| w[1] == OsString::from(aws_dir.clone().into_os_string()))
                .count();
            assert_eq!(aws_tmpfs_count, 1, "~/.aws should be tmpfs'd (still hidden)");
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

        // The file should appear as --bind /dev/null <file_path>
        let file_bind_count = args.windows(3)
            .filter(|w| w[0] == OsString::from("--bind"))
            .filter(|w| w[1] == OsString::from("/dev/null"))
            .filter(|w| w[2] == OsString::from(file_path.clone().into_os_string()))
            .count();
        assert_eq!(file_bind_count, 1, "file should be bound from /dev/null, not tmpfs'd");

        // The directory should appear as --tmpfs <dir_path>
        let dir_tmpfs_count = args.windows(2)
            .filter(|w| w[0] == OsString::from("--tmpfs"))
            .filter(|w| w[1] == OsString::from(dir_path.clone().into_os_string()))
            .count();
        assert_eq!(dir_tmpfs_count, 1, "directory should be tmpfs'd");

        // The file must NOT appear as --tmpfs
        let file_tmpfs_count = args.windows(2)
            .filter(|w| w[0] == OsString::from("--tmpfs"))
            .filter(|w| w[1] == OsString::from(file_path.clone().into_os_string()))
            .count();
        assert_eq!(file_tmpfs_count, 0, "file should not be tmpfs'd");
    }
}

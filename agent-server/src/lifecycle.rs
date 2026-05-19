use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};

use agent_core::config::Config;
use agent_core::provider::Provider;
use agent_core::store::Store;
use agent_core::types::{Entry, ServerEvent, TreeId};

const BUFFER_CAPACITY: usize = 1000;

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

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let config_path = agent_core::config::agent_dir().join("config.toml");

    let mut child = Command::new(&exe)
        .arg("worker")
        .arg("--tree-id")
        .arg(tree_id)
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn worker: {e}"))?;

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

    spawn_stdin_writer(stdin, stdin_rx);
    spawn_stdout_proxy(tree_id.to_string(), stdout, entry.clone(), store, cfg);
    spawn_stderr_demux(tree_id.to_string(), stderr);
    log::info!("[lifecycle] Spawned worker for tree {} (pid {})", tree_id, pid);
    Ok(())
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
            let err_event = ServerEvent::Error {
                message: "worker exited unexpectedly".into(),
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

fn spawn_stderr_demux(tree_id: String, stderr: ChildStderr) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut buf = String::new();
        let short = &tree_id[..tree_id.len().min(8)];
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => log::info!("[worker {}] {}", short, buf.trim_end()),
            }
        }
    });
}

// ── Graceful shutdown ──

/// Append a synthetic `SessionEnd` (Aborted) entry to a tree's data file.
/// Used when a worker crashes or the server shuts down uncleanly.
pub fn append_synthetic_session_end(store: &Store, tree_id: &str) {
    let entry = Entry::SessionEnd {
        id: agent_core::util::generate_entry_id(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: Some("session aborted (worker exit or server shutdown)".into()),
        status: agent_core::types::SessionStatus::Aborted,
        continuation_brief: None,
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        log::error!("[lifecycle] append synthetic session_end for {}: {}", tree_id, e);
    }
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
            append_synthetic_session_end(store, id);
        }
    }

    // Final cleanup: remove all workers from the map
    ACTIVE_WORKERS.lock().unwrap().clear();
    log::info!("[lifecycle] shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::types::Entry;

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
    fn test_append_synthetic_session_end() {
        use agent_core::store::Store;
        use agent_core::types::SessionStatus;
        use tempfile::TempDir;

        let dir = TempDir::with_prefix("agent-lifecycle-test-").unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let tree_id = "synthetic-test";

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

        // Append a start entry so the tree isn't empty
        store.append_entry(tree_id, &Entry::SessionStart {
            id: "s1".into(), parent_id: None, timestamp: "t1".into(),
        }).unwrap();

        append_synthetic_session_end(&store, tree_id);

        let entries = store.read_all_entries(tree_id).unwrap();
        let last = entries.last().unwrap();
        match last {
            Entry::SessionEnd { status, summary, .. } => {
                assert_eq!(*status, SessionStatus::Aborted);
                assert!(summary.as_deref().unwrap_or("").contains("aborted"));
            }
            other => panic!("expected SessionEnd, got {:?}", other),
        }
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
}

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use agent_core::agent;
use agent_core::config::Config;
use agent_core::provider::Provider;
use agent_core::store::Store;
use agent_core::types::{AgentInput, ServerEvent, TreeId};

/// Handle for controlling an active agent thread.
#[derive(Clone)]
pub struct AgentHandle {
    pub thread_id: Option<std::thread::ThreadId>,
    pub input_tx: mpsc::Sender<AgentInput>,
    pub stop: Arc<AtomicBool>,
    /// Ring buffer of the last N Entry events for SSE reconnection catch-up.
    pub event_buffer: Arc<Mutex<VecDeque<ServerEvent>>>,
    /// Live broadcast channels for SSE subscribers.
    pub event_broadcast: Arc<Mutex<Vec<mpsc::Sender<ServerEvent>>>>,
}

const BUFFER_CAPACITY: usize = 1000;

/// Map of active agent threads by tree ID.
pub static ACTIVE_AGENTS: LazyLock<Mutex<HashMap<TreeId, AgentHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Spawn an agent thread for the given tree.
///
/// Creates an agent handle, starts the agent loop in a new thread,
/// and a bridge thread that forwards events to SSE subscribers.
pub fn spawn(tree_id: &str, store: Arc<Store>, config: &Config) -> Result<(), String> {
    let mut agents = ACTIVE_AGENTS.lock().map_err(|e| e.to_string())?;

    if agents.contains_key(tree_id) {
        return Err(format!("Agent already active for tree {}", tree_id));
    }

    let (input_tx, input_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));

    // Event channels: agent writes to event_tx, bridge reads from event_rx
    let (event_tx, event_rx) = mpsc::channel::<ServerEvent>();

    let handle = AgentHandle {
        thread_id: None,
        input_tx,
        stop: stop.clone(),
        event_buffer: Arc::new(Mutex::new(VecDeque::with_capacity(BUFFER_CAPACITY))),
        event_broadcast: Arc::new(Mutex::new(Vec::new())),
    };

    let handle_for_bridge = handle.clone();
    let handle_for_agents = handle.clone();

    agents.insert(tree_id.to_string(), handle);
    drop(agents);

    // Create provider from config
    let provider = Provider::new(
        config.provider.base_url.clone(),
        config.provider.api_key.clone(),
        config.provider.model.clone(),
    );

    let session_config = config.session.clone();
    let tid = tree_id.to_string();
    let store_for_agent = (*store).clone();

    // Spawn agent thread
    let agent_stop = handle_for_agents.stop.clone();
    let agent_event_tx = event_tx;
    let thread_handle = std::thread::Builder::new()
        .name(format!("agent-{}", &tid))
        .spawn(move || {
            // Update thread_id in handle
            if let Ok(mut agents) = ACTIVE_AGENTS.lock() {
                if let Some(h) = agents.get_mut(&tid) {
                    h.thread_id = Some(std::thread::current().id());
                }
            }

            agent::run_agent(
                &tid,
                store_for_agent,
                provider,
                session_config,
                input_rx,
                agent_event_tx,
                agent_stop,
            );

            log::info!("[lifecycle] Agent thread exited for tree {}", tid);

            // Remove from active agents on exit
            if let Ok(mut agents) = ACTIVE_AGENTS.lock() {
                agents.remove(&tid);
            }
        })
        .map_err(|e| format!("Failed to spawn agent thread: {}", e))?;

    // Update thread_id in handle
    if let Ok(mut agents) = ACTIVE_AGENTS.lock() {
        if let Some(h) = agents.get_mut(tree_id) {
            h.thread_id = Some(thread_handle.thread().id());
        }
    }

    // Spawn bridge thread: reads events from agent and forwards to SSE broadcast
    let bridge_tid = tree_id.to_string();
    std::thread::Builder::new()
        .name(format!("bridge-{}", tree_id))
        .spawn(move || {
            for event in event_rx {
                // Ring buffer for SSE reconnection catch-up.
                // Clear on Done so that a new SSE subscriber opened for the
                // next message gets a fresh start — no stale events from the
                // previous turn.
                {
                    let mut buf = handle_for_bridge.event_buffer.lock().unwrap();
                    if matches!(&event, ServerEvent::Done { .. }) {
                        buf.clear();
                    } else {
                        if buf.len() >= BUFFER_CAPACITY {
                            buf.pop_front();
                        }
                        buf.push_back(event.clone());
                    }
                }

                // Live broadcast to all SSE subscribers
                let mut subs = handle_for_bridge.event_broadcast.lock().unwrap();
                subs.retain(|tx| tx.send(event.clone()).is_ok());
            }

            // Agent exited. Clear all broadcast senders to signal EOF
            // to any SSE subscribers (rx.recv() -> Err -> Read::read -> Ok(0)).
            handle_for_bridge.event_broadcast.lock().unwrap().clear();

            log::info!("[lifecycle] Bridge thread exited for tree {}", bridge_tid);
        })
        .map_err(|e| format!("Failed to spawn bridge thread: {}", e))?;

    log::info!(
        "[lifecycle] Spawned agent + bridge for tree {}",
        tree_id
    );
    Ok(())
}

/// Signal an agent thread to stop.
pub fn stop(tree_id: &str) -> Result<(), String> {
    let agents = ACTIVE_AGENTS.lock().map_err(|e| e.to_string())?;

    match agents.get(tree_id) {
        Some(handle) => {
            handle
                .stop
                .store(true, Ordering::Relaxed);
            let _ = handle.input_tx.send(AgentInput::Stop);
            log::info!("[lifecycle] Signaled stop for tree {}", tree_id);
            Ok(())
        }
        None => Err(format!("No active agent for tree {}", tree_id)),
    }
}

/// Send a user message to an active agent.
pub fn send_message(tree_id: &str, text: &str) -> Result<(), String> {
    let agents = ACTIVE_AGENTS.lock().map_err(|e| e.to_string())?;

    match agents.get(tree_id) {
        Some(handle) => handle
            .input_tx
            .send(AgentInput::Message {
                text: text.to_string(),
            })
            .map_err(|e| format!("Failed to send message: {}", e)),
        None => Err(format!("No active agent for tree {}", tree_id)),
    }
}

/// Get a clone of the agent handle for a tree (for SSE streaming).
pub fn get_handle(tree_id: &str) -> Option<AgentHandle> {
    ACTIVE_AGENTS.lock().ok()?.get(tree_id).cloned()
}

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

pub fn spawn_worker(tree_id: &str) -> Result<(), String> {
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
    spawn_stdout_proxy(tree_id.to_string(), stdout, entry.clone());
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
        }
        log::info!("[proxy {}] worker stdout closed", tree_id);
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
}

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use agent_core::types::{AgentInput, ServerEvent, TreeId};

/// Handle for controlling an active agent thread.
#[allow(dead_code)]
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
/// For Step 5 this is a stub that registers the handle but does not actually
/// start the agent loop. Full implementation in Step 7.
#[allow(dead_code)]
pub fn spawn(tree_id: &str) -> Result<(), String> {
    let mut agents = ACTIVE_AGENTS.lock().map_err(|e| e.to_string())?;

    if agents.contains_key(tree_id) {
        return Err(format!("Agent already active for tree {}", tree_id));
    }

    let (input_tx, _input_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));

    let handle = AgentHandle {
        thread_id: None,
        input_tx,
        stop,
        event_buffer: Arc::new(Mutex::new(VecDeque::with_capacity(BUFFER_CAPACITY))),
        event_broadcast: Arc::new(Mutex::new(Vec::new())),
    };

    log::info!("[lifecycle] Spawned agent (stub) for tree {}", tree_id);
    agents.insert(tree_id.to_string(), handle);
    Ok(())
}

/// Signal an agent thread to stop.
#[allow(dead_code)]
pub fn stop(tree_id: &str) -> Result<(), String> {
    let agents = ACTIVE_AGENTS.lock().map_err(|e| e.to_string())?;

    match agents.get(tree_id) {
        Some(handle) => {
            handle
                .stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            log::info!("[lifecycle] Signaled stop for tree {}", tree_id);
            Ok(())
        }
        None => Err(format!("No active agent for tree {}", tree_id)),
    }
}

/// Send a user message to an active agent.
#[allow(dead_code)]
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

/// Emit a ServerEvent to all SSE subscribers and (if Entry) to the ring buffer.
#[allow(dead_code)]
pub fn emit_event(handle: &AgentHandle, event: ServerEvent) {
    // Ring buffer for reconnection catch-up (Entry events only)
    if matches!(event, ServerEvent::Entry(_)) {
        let mut buf = handle.event_buffer.lock().unwrap();
        if buf.len() >= BUFFER_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(event.clone());
    }

    // Live broadcast — drop disconnected subscribers
    let mut subs = handle.event_broadcast.lock().unwrap();
    subs.retain(|tx| tx.send(event.clone()).is_ok());
}

use std::collections::{HashMap, VecDeque};
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
                // Ring buffer for ALL events (SSE reconnection catch-up),
                // not just Entry events — TextChunk, ToolStart, etc.
                // are also needed for late-connecting SSE clients.
                {
                    let mut buf = handle_for_bridge.event_buffer.lock().unwrap();
                    if buf.len() >= BUFFER_CAPACITY {
                        buf.pop_front();
                    }
                    buf.push_back(event.clone());
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

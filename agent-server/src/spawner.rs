//! Worker lifecycle management.
//!
//! Phase 2: async workers. Each active tree has a `WorkerHandle` with
//! `cmd_tx` (mpsc), `ev_tx` (broadcast), and `abort_tx` (watch).
//! No notify pipes, no nix::poll, no signal_hook.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::{broadcast, mpsc};

use agent_core::rpc::WsCommand;
use agent_core::types::ServerEvent;

use crate::server::WorkerHandle;

/// Global map of active workers, keyed by tree id.
pub static ACTIVE_WORKERS: LazyLock<Mutex<HashMap<String, WorkerHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Retrieve a clone of the WorkerHandle for a tree, if active.
pub fn worker_get(tree_id: &str) -> Option<WorkerHandle> {
    ACTIVE_WORKERS.lock().unwrap().get(tree_id).cloned()
}

/// Remove a worker entry (called by worker_task on exit).
pub fn worker_remove(tree_id: &str) {
    ACTIVE_WORKERS.lock().unwrap().remove(tree_id);
}

/// Send a Stop command to the worker for the given tree.
pub async fn worker_stop(tree_id: &str) -> Result<(), String> {
    let handle = worker_get(tree_id)
        .ok_or_else(|| format!("No active worker for {}", tree_id))?;
    handle
        .cmd_tx
        .send(WsCommand::Stop)
        .await
        .map_err(|e| format!("Failed to send Stop: {}", e))
}

/// Spawn a worker process + task for the given tree.
/// Returns a `WorkerHandle` for communication.
pub async fn spawn_worker(tree_id: &str, cfg: Arc<agent_core::config::Config>) -> WorkerHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(64);
    let (ev_tx, _) = broadcast::channel::<ServerEvent>(256);
    let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);

    let handle = WorkerHandle {
        cmd_tx: cmd_tx.clone(),
        ev_tx: ev_tx.clone(),
        abort: abort_tx,
    };

    // Register before spawning so WS clients can find it immediately.
    ACTIVE_WORKERS.lock().unwrap().insert(tree_id.to_string(), handle.clone());

    let tree_id = tree_id.to_string();
    let llm = crate::llm_client::LlmClient::new();
    let cfg = cfg.clone();

    tokio::spawn(async move {
        // Abort watcher: if abort_tx sends true, the worker task exits.
        let mut abort_rx = abort_rx;
        let cmd_rx = cmd_rx;
        let ev_tx = ev_tx;

        tokio::select! {
            _ = abort_rx.changed() => {
                log::debug!("[spawner] worker {} aborted", tree_id);
            }
            _ = crate::worker_task::run_worker_task(tree_id.clone(), cfg, llm, cmd_rx, ev_tx) => {}
        }
    });

    handle
}

/// Broadcast a MetaUpdate event to a worker's WS clients.
pub fn broadcast_meta_update(tree_id: &str, title: Option<String>) {
    if let Some(handle) = worker_get(tree_id) {
        let _ = handle
            .ev_tx
            .send(ServerEvent::MetaUpdate { title, model: None });
    }
}

/// Shut down all active workers gracefully, then force-kill stragglers.
pub async fn shutdown_all() {
    let snapshot: Vec<String> = ACTIVE_WORKERS.lock().unwrap().keys().cloned().collect();
    if snapshot.is_empty() {
        log::info!("[spawner] no active workers to shut down");
        return;
    }
    log::info!("[spawner] shutting down {} worker(s)", snapshot.len());

    // Send Stop to all
    for id in &snapshot {
        let _ = worker_stop(id).await;
    }

    // Wait briefly for them to exit, then abort (abort kills the child)
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    for id in &snapshot {
        let exists = ACTIVE_WORKERS.lock().unwrap().contains_key(id);
        if exists {
            log::warn!("[spawner] worker {} still alive after 5s, aborting", id);
            if let Some(handle) = worker_get(id) {
                let _ = handle.abort.send(true);
            }
        }
    }

    // Final cleanup
    ACTIVE_WORKERS.lock().unwrap().clear();
    log::info!("[spawner] shutdown complete");
}

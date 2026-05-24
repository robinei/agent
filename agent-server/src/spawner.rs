use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};

use agent_core::config::Config;
use agent_core::types::{ServerEvent, TreeId};

use crate::worker_loop;

// ── WorkerMsg ──

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WorkerMsg {
    NewClient(Box<worker_loop::WsClient>),
    InjectEvent(ServerEvent),
    Stop,
}

// ── Worker entry ──

pub struct WorkerEntry {
    pub pid: u32,
    pub msg_tx: mpsc::SyncSender<WorkerMsg>,
    pub notify_write: std::fs::File,
}

pub static ACTIVE_WORKERS: LazyLock<Mutex<HashMap<TreeId, Arc<Mutex<WorkerEntry>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn worker_get(tree_id: &str) -> Option<Arc<Mutex<WorkerEntry>>> {
    ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tree_id)
        .cloned()
}

/// Broadcast a MetaUpdate event by sending a message to the worker's event loop.
pub fn broadcast_meta_update(tree_id: &str, title: Option<String>) {
    let entry = match worker_get(tree_id) {
        Some(e) => e,
        None => return,
    };
    let guard = entry.lock().unwrap_or_else(|e| e.into_inner());
    let _ = guard
        .msg_tx
        .send(WorkerMsg::InjectEvent(ServerEvent::MetaUpdate { title }));
    let _ = nix::unistd::write(&guard.notify_write, b"\x00");
}

fn agent_dir() -> PathBuf {
    agent_core::config::agent_dir()
}

pub fn spawn_worker(tree_id: &str, cfg: Arc<Config>) -> Result<(), String> {
    let workers = ACTIVE_WORKERS.lock().map_err(|e| e.to_string())?;
    if workers.contains_key(tree_id) {
        return Err(format!("Worker already active for tree {}", tree_id));
    }
    drop(workers);

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

    let meta = agent_core::tree_io::read_meta(&agent_dir(), tree_id)
        .map_err(|e| format!("read_meta: {e}"))?
        .ok_or_else(|| format!("tree {} not found", tree_id))?;

    let (msg_tx, msg_rx) = mpsc::sync_channel::<WorkerMsg>(64);
    let (notify_read, notify_write) = {
        let (r, w) = nix::unistd::pipe().map_err(|e| format!("pipe: {e}"))?;
        // Read end must be non-blocking so NotifyHandler's drain loop gets EAGAIN
        // when the pipe is empty, rather than blocking forever after the first byte.
        use std::os::fd::AsRawFd;
        nix::fcntl::fcntl(
            r.as_raw_fd(),
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .map_err(|e| format!("notify_read set_nonblocking: {e}"))?;
        (
            unsafe { std::fs::File::from_raw_fd(r.into_raw_fd()) },
            unsafe { std::fs::File::from_raw_fd(w.into_raw_fd()) },
        )
    };

    let stderr_buf: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(20)));

    let (spawn_tx, spawn_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    let tree_id_str = tree_id.to_string();
    std::thread::spawn({
        let cfg = cfg.clone();
        let stderr_buf = stderr_buf.clone();
        move || {
            // Spawn the subprocess (bwrap or direct)
            let mut child = if cfg.sandbox.enabled {
                let bwrap_path = match crate::sandbox::resolve_bwrap_path(&cfg.sandbox.bwrap_path)
                {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = spawn_tx.send(Err(e));
                        return;
                    }
                };
                let bwrap_args = crate::sandbox::build_bwrap_argv(
                    &exe,
                    &tree_id_str,
                    &meta,
                    &cfg,
                );
                match Command::new(&bwrap_path)
                    .args(&bwrap_args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = spawn_tx.send(Err(format!("spawn bwrap worker: {e}")));
                        return;
                    }
                }
            } else {
                match Command::new(&exe)
                    .arg("worker")
                    .arg("--tree-id")
                    .arg(&tree_id_str)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = spawn_tx.send(Err(format!("spawn worker: {e}")));
                        return;
                    }
                }
            };

            let pid = child.id();
            let mut child_stdin = child.stdin.take().unwrap();
            let child_stdout = child.stdout.take().unwrap();
            let child_stderr = child.stderr.take().unwrap();

            // Non-blocking so StdoutHandler can drain all buffered lines
            // in a single on_ready call without blocking on an empty pipe.
            use std::os::fd::AsRawFd;
            nix::fcntl::fcntl(
                child_stdout.as_raw_fd(),
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            )
            .ok();

            // Insert into ACTIVE_WORKERS before starting the stdin writer
            // so worker_subscribe-like paths can find the entry immediately.
            let entry = Arc::new(Mutex::new(WorkerEntry {
                pid,
                msg_tx: msg_tx.clone(),
                notify_write: notify_write
                    .try_clone()
                    .expect("clone notify_write"),
            }));
            ACTIVE_WORKERS
                .lock()
                .unwrap()
                .insert(tree_id_str.clone(), entry);

            log::debug!(
                "[spawner] Spawned worker for tree {} (pid {}) model={} thinking={}",
                tree_id_str,
                pid,
                cfg.provider.model,
                cfg.provider.enable_thinking
            );

            // Send initial config as the first message on stdin.
            let worker_cfg = agent_core::rpc::WorkerConfig {
                session_soft_cap_pct: cfg.session.soft_cap_pct,
                session_hard_cap_pct: cfg.session.hard_cap_pct,
                max_tool_calls_per_turn: cfg.session.max_tool_calls_per_turn,
                logging_level: cfg.logging.level.clone(),
                logging_to_file: cfg.logging.to_file.clone(),
                logging_to_stderr: cfg.logging.to_stderr,
                lsp: cfg.lsp.clone(),
            };
            let config_msg =
                serde_json::to_string(&agent_core::rpc::PipeIn::Config(worker_cfg)).unwrap();
            if let Err(e) = writeln!(&mut child_stdin, "{}", config_msg)
                .and_then(|_| child_stdin.flush())
            {
                log::error!("[spawner] write worker config for {}: {}", tree_id_str, e);
            }

            // Run the event loop inline — this keeps the current thread alive
            // until the worker exits, which is what we need for bwrap's PDEATHSIG.
            worker_loop::run_event_loop(
                tree_id_str,
                child_stdin,
                child_stdout,
                child_stderr,
                msg_rx,
                notify_read,
                notify_write,
                cfg,
                stderr_buf,
                spawn_tx,
                child,
            );
        }
    });

    match spawn_rx.recv() {
        Ok(result) => result,
        Err(_) => Err("worker keeper thread exited without signaling".into()),
    }
}

pub fn worker_stop(tree_id: &str) -> Result<(), String> {
    let entry = worker_get(tree_id).ok_or_else(|| format!("No active worker for tree {}", tree_id))?;
    let guard = entry.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .msg_tx
        .send(WorkerMsg::Stop)
        .map_err(|e| format!("Failed to send stop: {}", e))?;
    let _ = nix::unistd::write(&guard.notify_write, b"\x00");
    Ok(())
}

pub fn shutdown_all() {
    let snapshot: Vec<(String, u32)> = {
        let map = ACTIVE_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .map(|(id, entry)| {
                let pid = entry.lock().unwrap_or_else(|e| e.into_inner()).pid;
                (id.clone(), pid)
            })
            .collect()
    };
    if snapshot.is_empty() {
        log::info!("[spawner] no active workers to shut down");
        return;
    }
    log::info!("[spawner] shutting down {} worker(s)", snapshot.len());

    for (id, _) in &snapshot {
        let _ = worker_stop(id);
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    for (id, pid) in &snapshot {
        loop {
            if std::time::Instant::now() > deadline {
                log::warn!("[spawner] worker {} still alive after 60s, killing", id);
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(*pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                break;
            }
            let gone = !ACTIVE_WORKERS
                .lock()
                .unwrap()
                .contains_key(id);
            if gone {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    log::info!("[spawner] shutdown complete");
}
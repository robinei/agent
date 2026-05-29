//! Async worker task — replaces `worker_loop.rs` + `worker_ctx.rs`.
//!
//! Runs one per active tree. Reads child stdout via the event-funnel pattern:
//! a forwarder task reads complete JSON lines and sends them into an mpsc,
//! while the main loop `recv()`s a unified `WorkerEvent`.

use std::process::Stdio;
use std::sync::Arc;

use agent_core::child_io::ChildLines;
use agent_core::config::Config;
use agent_core::rpc::{LlmRequest, PipeIn, PipeOut, WsCommand};
use agent_core::types::{Entry, ServerEvent, SessionStatus};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc};

use crate::llm_client::LlmClient;
use crate::spawner;

/// Unified event from the funnel — either child stdout, a command, or child exit.
enum WorkerEvent {
    Stdout(PipeOut),
    Command(WsCommand),
    ChildExited(std::process::ExitStatus),
}

/// Run the async worker event loop for a single tree.
///
/// Spawns the child process, sets up the event-funnel forwarders, and runs
/// the main event loop until the child exits or the worker is stopped.
pub async fn run_worker_task(
    tree_id: String,
    cfg: Arc<Config>,
    llm: LlmClient,
    cmd_rx: mpsc::Receiver<WsCommand>,
    ev_tx: broadcast::Sender<ServerEvent>,
) {
    // ── 1. Spawn the worker process ──
    let exe = std::env::args()
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("agent"));

    let mut child = match Command::new(&exe)
        .arg("worker")
        .arg("--tree-id")
        .arg(&tree_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log::error!("[worker_task {}] spawn failed: {}", tree_id, e);
            let _ = ev_tx.send(ServerEvent::Notification {
                level: agent_core::types::NotificationLevel::Fatal,
                message: format!("worker spawn failed: {}", e),
            });
            let _ = ev_tx.send(ServerEvent::Done {
                status: "aborted".into(),
                usage: None,
            });
            return;
        }
    };

    let pid = child.id().unwrap_or(0);
    let child_stdin = child.stdin.take().expect("take stdin");
    let child_stdout = child.stdout.take().expect("take stdout");
    let child_stderr = child.stderr.take().expect("take stderr");

    log::debug!("[worker_task {}] spawned worker (pid {})", tree_id, pid);

    // Send initial Config to the worker.
    let worker_cfg = agent_core::rpc::WorkerConfig {
        session_soft_cap_pct: cfg.session.soft_cap_pct,
        session_hard_cap_pct: cfg.session.hard_cap_pct,
        max_tool_calls_per_turn: cfg.session.max_tool_calls_per_turn,
        logging_level: cfg.logging.level.clone(),
        logging_to_file: cfg.logging.to_file.clone(),
        logging_to_stderr: cfg.logging.to_stderr,
        lsp: cfg.lsp.clone(),
    };
    let config_msg = serde_json::to_string(&PipeIn::Config(worker_cfg)).unwrap();
    let mut stdin_writer = tokio::io::BufWriter::new(child_stdin);
    let _ = stdin_writer.write_all(format!("{}\n", config_msg).as_bytes()).await;
    let _ = stdin_writer.flush().await;

    // ── 2. Set up the event funnel ──
    //
    // INTENTIONAL: each forwarder owns its reader and emits only complete
    // items, so no future is ever dropped mid-read. This is cancellation-safe
    // by construction — we never poll `read_line` inside a `select!`.

    let (worker_tx, mut worker_rx) = mpsc::channel::<WorkerEvent>(256);

    // Forwarder: child stdout → WorkerEvent::Stdout
    let stdout_tree_id = tree_id.clone();
    let stdout_tx = worker_tx.clone();
    let stdout_handle = tokio::spawn(async move {
        let mut lines = ChildLines::new(child_stdout);
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    match serde_json::from_str::<PipeOut>(&line) {
                        Ok(pipe) => {
                            if stdout_tx.send(WorkerEvent::Stdout(pipe)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "[worker_task {}] bad PipeOut JSON ({}): {}",
                                stdout_tree_id,
                                e,
                                line
                            );
                        }
                    }
                }
                Ok(None) => break,  // EOF
                Err(e) => {
                    log::warn!("[worker_task {}] stdout read error: {}", stdout_tree_id, e);
                    break;
                }
            }
        }
    });

    // Forwarder: child stderr → log (no funnel, just background consume)
    let stderr_tree_id = tree_id.clone();
    let stderr_handle = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let reader = tokio::io::BufReader::new(child_stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let short = &stderr_tree_id[..stderr_tree_id.len().min(8)];
            log::debug!("[worker {}] {}", short, line);
        }
    });

    // Forwarder: commands from WS clients → WorkerEvent::Command
    // (We integrate this into the main loop via `select!` rather than a
    // separate forwarder, since mpsc::Receiver::recv() is cancel-safe.)

    // ── 3. PipeIn writer task (serializes writes to child stdin) ──
    let (pipein_tx, mut pipein_rx) = mpsc::channel::<PipeIn>(256);
    let pipein_handle = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut buf = String::new();
        // Actually just a writer task
        let mut writer = stdin_writer;
        while let Some(msg) = pipein_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    if writer.write_all(format!("{}\n", json).as_bytes()).await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("[worker_task] serialize PipeIn: {}", e);
                }
            }
        }
    });

    // ── 4. Main event loop ──
    //
    // Reads from three sources:
    //   a) worker_rx — events from child stdout forwarder
    //   b) cmd_rx — commands from WS clients
    //   c) child exit (via wait())
    //
    // PipeOut::Llm → spawn a reqwest stream task, write PipeIn::Llm back
    // PipeOut::Event → broadcast to ev_tx
    // WsCommand → serialize as PipeIn and send to pipein_tx

    let mut cmd_rx = cmd_rx;

    loop {
        tokio::select! {
            // (a) Child stdout events
            Some(event) = worker_rx.recv() => {
                match event {
                    WorkerEvent::Stdout(PipeOut::Event(event)) => {
                        let is_done = matches!(&event, ServerEvent::Done { .. });
                        let _ = ev_tx.send(event);
                        if is_done {
                            // After Done, send AutoTitle command
                            let _ = pipein_tx.send(PipeIn::Cmd(WsCommand::AutoTitle)).await;
                        }
                    }
                    WorkerEvent::Stdout(PipeOut::Llm(req)) => {
                        // Spawn an LLM streaming task — futures are Send, so
                        // plain tokio::spawn works (no LocalSet needed).
                        let llm = llm.clone();
                        let cfg = cfg.clone();
                        let pipein_tx = pipein_tx.clone();
                        tokio::spawn(async move {
                            let (resp_tx, mut resp_rx) = mpsc::channel::<agent_core::rpc::LlmResponse>(16);
                            let result = llm.stream_completion(&req, &cfg.provider, &resp_tx).await;
                            // Drain remaining responses
                            while let Some(resp) = resp_rx.recv().await {
                                let _ = pipein_tx.send(PipeIn::Llm(resp)).await;
                            }
                            // Stream errored — send an error response
                            if let Err(e) = result {
                                let _ = pipein_tx.send(PipeIn::Llm(
                                    agent_core::rpc::LlmResponse::Error {
                                        id: req.id,
                                        message: e.to_string(),
                                    }
                                )).await;
                            }
                        });
                    }
                    WorkerEvent::Command(cmd) => {
                        let _ = pipein_tx.send(PipeIn::Cmd(cmd)).await;
                    }
                    WorkerEvent::ChildExited(_) => {
                        // Handled below via child.wait()
                    }
                }
            }

            // (b) WS commands
            Some(cmd) = cmd_rx.recv() => {
                let _ = pipein_tx.send(PipeIn::Cmd(cmd)).await;
            }

            // (c) Child exit
            status = child.wait() => {
                match status {
                    Ok(status) if status.success() => {
                        log::debug!("[worker_task {}] worker exited cleanly", tree_id);
                    }
                    Ok(status) => {
                        use std::os::unix::process::ExitStatusExt;
                        let desc = status.code().map(|c| format!(" (exit {})", c))
                            .or_else(|| status.signal().map(|s| format!(" (signal {})", s)))
                            .unwrap_or_default();
                        log::warn!("[worker_task {}] worker exited{}", tree_id, desc);
                        let _ = ev_tx.send(ServerEvent::Notification {
                            level: agent_core::types::NotificationLevel::Fatal,
                            message: format!("worker exited unexpectedly{}", desc),
                        });
                        let _ = ev_tx.send(ServerEvent::Done {
                            status: "aborted".into(),
                            usage: None,
                        });
                    }
                    Err(e) => {
                        log::warn!("[worker_task {}] wait error: {}", tree_id, e);
                    }
                }
                break;
            }
        }
    }

    // ── 5. Cleanup ──
    // Abort the forwarder tasks (they'll stop on their own once the
    // mpsc sender is dropped, but this is cleaner).
    stdout_handle.abort();
    stderr_handle.abort();
    pipein_handle.abort();

    // Remove from active workers
    spawner::worker_remove(&tree_id);

    log::debug!("[worker_task {}] worker task finished", tree_id);
}

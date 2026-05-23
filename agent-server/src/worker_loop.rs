use std::any::TypeId;
use std::collections::VecDeque;
use std::os::fd::BorrowedFd;
use std::process::Child;
use std::sync::mpsc;
use std::sync::Arc;

use agent_core::config::Config;
use agent_core::store::Store;
use nix::poll::{PollFd, PollFlags};

pub use crate::handlers::StderrBuf;
pub use crate::worker_ctx::{PollHandler, WorkerCtx};
pub use crate::ws_client::WsClient;

use crate::handlers::{NotifyHandler, StderrHandler, StdoutHandler};
use crate::lifecycle;
use crate::lifecycle::WorkerMsg;

pub fn run_event_loop(
    tree_id: String,
    child_stdin: std::process::ChildStdin,
    child_stdout: std::process::ChildStdout,
    child_stderr: std::process::ChildStderr,
    msg_rx: mpsc::Receiver<WorkerMsg>,
    notify_read: std::fs::File,
    notify_write: std::fs::File,
    store: Arc<Store>,
    cfg: Arc<Config>,
    stderr_buf: StderrBuf,
    spawn_tx: mpsc::SyncSender<Result<(), String>>,
    mut child: Child,
) {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    );

    let _ = spawn_tx.send(Ok(()));

    let mut ctx = WorkerCtx {
        tree_id: tree_id.clone(),
        stdin: std::io::BufWriter::new(child_stdin),
        ws_clients: Vec::new(),
        event_buffer: VecDeque::with_capacity(1000),
        store,
        cfg,
        msg_rx,
        tls_config,
        new_handlers: Vec::new(),
    };

    let mut handlers: Vec<Box<dyn PollHandler>> = Vec::new();
    handlers.push(Box::new(StdoutHandler::new(child_stdout)));
    handlers.push(Box::new(StderrHandler::new(
        child_stderr,
        stderr_buf.clone(),
    )));
    handlers.push(Box::new(NotifyHandler::new(notify_read)));

    let _notify_keepalive = notify_write;

    let stdout_type_id = TypeId::of::<StdoutHandler>();

    loop {
        let mut pollfds: Vec<PollFd> = handlers
            .iter()
            .map(|h| {
                let fd = h.fd();
                let flags = h.interests();
                // SAFETY: Each handler owns its fd and lives for the duration of the loop.
                PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, flags)
            })
            .collect();

        for c in &ctx.ws_clients {
            // SAFETY: ws_clients owns the WsClient objects, which hold the fd.
            pollfds.push(PollFd::new(
                unsafe { BorrowedFd::borrow_raw(c.fd()) },
                PollFlags::POLLIN,
            ));
        }

        match nix::poll::poll(&mut pollfds, 30_000u16) {
            Ok(_) => {}
            Err(e) => {
                log::error!("[worker_loop {}] poll error: {}", tree_id, e);
                break;
            }
        }

        // Dispatch pollable handlers (index-synced with pollfds)
        let mut handler_count = handlers.len();
        let mut i = 0;
        while i < handler_count {
            let revents = pollfds[i].revents().unwrap_or(PollFlags::empty());
            if !revents.is_empty() && !handlers[i].on_ready(&mut ctx) {
                handlers.swap_remove(i);
                pollfds.swap_remove(i);
                handler_count -= 1;
                continue;
            }
            i += 1;
        }

        // Truncate stale pollfds entries from swap_remove
        pollfds.truncate(handler_count);

        // WS clients: try non-blocking read on every iteration
        ctx.retain_ws_clients(|c, stdin| c.on_readable(stdin));

        // Attach newly-created handlers (e.g. LlmHandler)
        handlers.append(&mut ctx.new_handlers);

        // WS client keepalive / timeout
        ctx.retain_ws_clients(|c, stdin| c.tick(stdin));

        // Exit when worker stdout closes
        if !handlers.iter().any(|h| h.type_id() == stdout_type_id) {
            log::debug!("[worker_loop {}] worker stdout closed", tree_id);
            break;
        }
    }

    // Crash detection
    let (exit_ok, exit_desc) = match child.wait() {
        Ok(status) if status.success() => (true, String::new()),
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            let desc = if let Some(code) = status.code() {
                format!(" (exit code {})", code)
            } else if let Some(sig) = status.signal() {
                format!(" (killed by signal {})", sig)
            } else {
                String::new()
            };
            (false, desc)
        }
        Err(e) => (false, format!(" (wait error: {})", e)),
    };

    if !exit_ok {
        log::warn!(
            "[worker_loop {}] worker exited with error{}",
            tree_id,
            exit_desc
        );
        let detail = {
            let g = stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_empty() {
                String::new()
            } else {
                format!("\n{}", g.iter().cloned().collect::<Vec<_>>().join("\n"))
            }
        };
        ctx.broadcast(agent_core::types::ServerEvent::Notification {
            level: agent_core::types::NotificationLevel::Fatal,
            message: format!("worker exited unexpectedly{}{}", exit_desc, detail),
        });
        ctx.broadcast(agent_core::types::ServerEvent::Done {
            status: "aborted".into(),
        });
    }

    lifecycle::ACTIVE_WORKERS.lock().unwrap_or_else(|e| e.into_inner()).remove(&tree_id);
}

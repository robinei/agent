use std::collections::VecDeque;
use std::io::BufRead;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::process::{ChildStderr, ChildStdout};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use agent_core::rpc::PipeIn;
use agent_core::types::ServerEvent;
use nix::poll::PollFlags;

use crate::lifecycle::WorkerMsg;
use crate::worker_ctx::{PollHandler, WorkerCtx};

pub type StderrBuf = Arc<Mutex<VecDeque<String>>>;

pub struct StdoutHandler {
    reader: std::io::BufReader<ChildStdout>,
    line_buf: String,
}

impl StdoutHandler {
    pub fn new(stdout: ChildStdout) -> Self {
        Self {
            reader: std::io::BufReader::new(stdout),
            line_buf: String::new(),
        }
    }
}

impl PollHandler for StdoutHandler {
    fn fd(&self) -> RawFd {
        self.reader.get_ref().as_raw_fd()
    }

    fn interests(&self) -> PollFlags {
        PollFlags::POLLIN
    }

    fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool {
        loop {
            // Do NOT clear line_buf here — partial reads must survive across
            // on_ready calls. When a JSON line exceeds the pipe buffer (64KB),
            // read_line returns WouldBlock mid-line; the accumulated bytes must
            // still be in line_buf when on_ready is called again after POLLIN.
            match self.reader.read_line(&mut self.line_buf) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) => {
                    log::warn!("[worker_loop {}] stdout read error: {}", ctx.tree_id, e);
                    return false;
                }
            }
            let trimmed = self.line_buf.trim_end();
            let pipe_out: agent_core::rpc::PipeOut = match serde_json::from_str(trimmed) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!(
                        "[worker_loop {}] bad PipeOut JSON ({}): {}",
                        ctx.tree_id,
                        e,
                        trimmed
                    );
                    self.line_buf.clear();
                    continue;
                }
            };
            self.line_buf.clear();
            match pipe_out {
                agent_core::rpc::PipeOut::Event(event) => {
                    if matches!(event, ServerEvent::Done { .. }) {
                        crate::lifecycle::spawn_auto_title(ctx);
                    }
                    ctx.broadcast(event);
                }
                agent_core::rpc::PipeOut::Llm(req) => {
                    log::debug!(
                        "[worker_loop {}] creating LlmHandler for req_id={}",
                        ctx.tree_id,
                        req.id
                    );
                    match crate::llm_handler::LlmHandler::new(req, &ctx.cfg, ctx.tls_config.clone())
                    {
                        Ok(handler) => ctx.new_handlers.push(Box::new(handler)),
                        Err(e) => {
                            log::warn!(
                                "[worker_loop {}] LlmHandler::new failed: {}",
                                ctx.tree_id,
                                e
                            );
                            crate::llm_handler::send_llm_error(
                                ctx,
                                0,
                                &format!("LLM connection failed: {e}"),
                            );
                        }
                    }
                }
            }
        }
    }
}

pub struct StderrHandler {
    reader: std::io::BufReader<ChildStderr>,
    line_buf: String,
    buf: StderrBuf,
}

impl StderrHandler {
    pub fn new(stderr: ChildStderr, buf: StderrBuf) -> Self {
        Self {
            reader: std::io::BufReader::new(stderr),
            line_buf: String::new(),
            buf,
        }
    }
}

impl PollHandler for StderrHandler {
    fn fd(&self) -> RawFd {
        self.reader.get_ref().as_raw_fd()
    }

    fn interests(&self) -> PollFlags {
        PollFlags::POLLIN
    }

    fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool {
        // Stderr fd is blocking (unlike stdout), so we read exactly one line
        // per POLLIN event. Looping would block on the second read_line call
        // when there is no more data, freezing the entire event loop.
        self.line_buf.clear();
        match self.reader.read_line(&mut self.line_buf) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        let trimmed = self.line_buf.trim_end().to_string();
        let short = &ctx.tree_id[..ctx.tree_id.len().min(8)];
        log::debug!("[worker {}] {}", short, trimmed);
        let mut g = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= 20 {
            g.pop_front();
        }
        g.push_back(trimmed);
        true
    }
}

pub struct NotifyHandler {
    notify_read: std::fs::File,
}

impl NotifyHandler {
    pub fn new(notify_read: std::fs::File) -> Self {
        Self { notify_read }
    }
}

impl PollHandler for NotifyHandler {
    fn fd(&self) -> RawFd {
        self.notify_read.as_raw_fd()
    }

    fn interests(&self) -> PollFlags {
        PollFlags::POLLIN
    }

    fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool {
        loop {
            let mut buf = [0u8; 64];
            match nix::unistd::read(self.notify_read.as_raw_fd(), &mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(_) => break,
            }
        }
        loop {
            match ctx.msg_rx.try_recv() {
                Ok(WorkerMsg::NewClient(mut ws_client)) => {
                    for ev in &ctx.event_buffer {
                        let json = serde_json::to_string(ev).unwrap_or_default();
                        let _ = ws_client.write_raw(&json);
                    }
                    ctx.ws_clients.push(*ws_client);
                }
                Ok(WorkerMsg::InjectEvent(ev)) => {
                    ctx.broadcast(ev);
                }
                Ok(WorkerMsg::Stop) => {
                    let pipe_in = PipeIn::Cmd(agent_core::rpc::WsCommand::Stop);
                    ctx.send_pipe_in(&pipe_in);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        true
    }
}
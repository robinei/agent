use std::any::TypeId;
use std::io::{BufWriter, Write};
use std::os::fd::RawFd;
use std::process::ChildStdin;
use std::sync::mpsc;
use std::sync::Arc;

use agent_core::config::Config;
use agent_core::rpc::PipeIn;
use agent_core::types::ServerEvent;
use nix::poll::PollFlags;

use crate::spawner::WorkerMsg;
use crate::ws_client::WsClient;

pub trait PollHandler
where
    Self: 'static,
{
    fn fd(&self) -> RawFd;
    fn interests(&self) -> PollFlags;
    fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool;
    fn type_id(&self) -> TypeId {
        TypeId::of::<Self>()
    }
}

pub struct WorkerCtx {
    pub tree_id: String,
    pub stdin: BufWriter<ChildStdin>,
    pub ws_clients: Vec<WsClient>,
    pub cfg: Arc<Config>,
    pub msg_rx: mpsc::Receiver<WorkerMsg>,
    pub tls_config: Arc<rustls::ClientConfig>,
    pub new_handlers: Vec<Box<dyn PollHandler>>,
}

impl WorkerCtx {
    pub fn broadcast(&mut self, ev: ServerEvent) {
        log::debug!(
            "[worker_loop {}] broadcast: {} to {} client(s)",
            self.tree_id,
            match &ev {
                ServerEvent::TextChunk { content } => format!("TextChunk(len={})", content.len()),
                ServerEvent::ThinkingChunk { content } => {
                    format!("ThinkingChunk(len={})", content.len())
                }
                ServerEvent::ToolStart { tool, .. } => format!("ToolStart({})", tool),
                ServerEvent::ToolResult { tool, exit, .. } => {
                    format!("ToolResult({}, exit={})", tool, exit)
                }
                ServerEvent::Entry(e) => format!("Entry({})", e.id()),
                ServerEvent::ContextUpdate { status, pct, .. } => format!("ContextUpdate({:?},{}%)", status, pct),
                ServerEvent::Notification { level, message } => {
                    format!("Notification({:?}, {})", level, message)
                }
                ServerEvent::Done { status, .. } => format!("Done({})", status),
                ServerEvent::FileChanged { path, kind } => format!("FileChanged({},{})", kind, path),
                ServerEvent::MetaUpdate { .. } => "MetaUpdate".into(),
                ServerEvent::Diagnostics { source, files } => {
                    let total: usize = files.iter().map(|f| f.diagnostics.len()).sum();
                    format!("Diagnostics({}, {} diags)", source, total)
                }
            },
            self.ws_clients.len()
        );
        let json = serde_json::to_string(&ev).unwrap_or_default();
        let mut i = 0;
        while i < self.ws_clients.len() {
            if !self.ws_clients[i].write_raw(&json) {
                self.ws_clients.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    pub fn stdin_send(&mut self, json_line: &str) {
        let _ = writeln!(self.stdin, "{}", json_line);
        let _ = self.stdin.flush();
    }

    pub fn send_pipe_in(&mut self, msg: &PipeIn) {
        if let Ok(json) = serde_json::to_string(msg) {
            self.stdin_send(&json);
        }
    }

    pub fn retain_ws_clients(
        &mut self,
        mut f: impl FnMut(&mut WsClient, &mut BufWriter<ChildStdin>) -> bool,
    ) {
        let mut i = 0;
        while i < self.ws_clients.len() {
            if !f(&mut self.ws_clients[i], &mut self.stdin) {
                self.ws_clients.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}
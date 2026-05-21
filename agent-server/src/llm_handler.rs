use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::sync::Arc;

use agent_core::config::Config;
use agent_core::rpc::{LlmRequest, LlmResponse, PipeIn};

use crate::provider::Provider;
use nix::poll::PollFlags;
use rustls::pki_types::ServerName;

use crate::worker_ctx::{PollHandler, WorkerCtx};

// ── Transport ──

enum LlmTransport {
    Plain(TcpStream),
    Tls {
        tcp: TcpStream,
        conn: rustls::ClientConnection,
    },
}

impl LlmTransport {
    fn fd(&self) -> RawFd {
        match self {
            LlmTransport::Plain(tcp) | LlmTransport::Tls { tcp, .. } => tcp.as_raw_fd(),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            LlmTransport::Plain(tcp) => tcp.read(buf),
            LlmTransport::Tls { conn, .. } => conn.reader().read(buf),
        }
    }

    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            LlmTransport::Plain(tcp) => tcp.write(buf),
            LlmTransport::Tls { conn, .. } => conn.writer().write(buf),
        }
    }
}

// ── State machine ──

enum LlmState {
    SendRequest { body: Vec<u8>, sent: usize },
    ReadHeaders { buf: Vec<u8> },
    Streaming,
}

// ── LlmHandler ──

pub struct LlmHandler {
    transport: LlmTransport,
    state: LlmState,
    req_id: u64,
    line_buf: String,
}

impl LlmHandler {
    pub fn new(
        req: LlmRequest,
        cfg: &Config,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Result<Self, String> {
        let base_url = cfg.provider.base_url.trim_end_matches('/');
        let (host, port, base_path) = parse_host_port_path(base_url)?;
        let path = format!("{}/chat/completions", base_path);
        let addr = format!("{}:{}", host, port);
        let is_tls = base_url.starts_with("https://");

        let tcp = TcpStream::connect(&addr).map_err(|e| format!("connect to {addr}: {e}"))?;
        tcp.set_nonblocking(true)
            .map_err(|e| format!("set nonblocking: {e}"))?;

        let transport = if is_tls {
            let server_name =
                ServerName::try_from(host.clone()).map_err(|e| format!("bad hostname: {e}"))?;
            let conn = rustls::ClientConnection::new(tls_config, server_name)
                .map_err(|e| format!("rustls connect: {e}"))?;
            LlmTransport::Tls { tcp, conn }
        } else {
            LlmTransport::Plain(tcp)
        };

        let provider = Provider::new(
            cfg.provider.base_url.clone(),
            cfg.provider.api_key.clone(),
            cfg.provider.model.clone(),
            cfg.provider.enable_thinking,
            cfg.provider.reasoning_effort.clone(),
            cfg.provider.max_tokens,
            cfg.provider.sort.clone(),
        );
        let body = provider.build_body(&req.messages, &req.tools, true);
        let body_str = serde_json::to_string(&body).map_err(|e| format!("json body: {e}"))?;
        let request = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Content-Type: application/json\r\n\
             Authorization: Bearer {}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            cfg.provider.api_key,
            body_str.len(),
            body_str
        );

        Ok(Self {
            transport,
            state: LlmState::SendRequest {
                body: request.into_bytes(),
                sent: 0,
            },
            req_id: req.id,
            line_buf: String::new(),
        })
    }

    fn do_tls_io(&mut self, ctx: &mut WorkerCtx) -> bool {
        if let LlmTransport::Tls { tcp, conn } = &mut self.transport {
            if conn.wants_read() {
                let n = conn.read_tls(tcp).unwrap_or(0);
                log::debug!("[LlmHandler {}] TLS read_tls={}", ctx.tree_id, n);
            }
            if conn.wants_write() {
                let n = conn.write_tls(tcp).unwrap_or(0);
                log::debug!("[LlmHandler {}] TLS write_tls={}", ctx.tree_id, n);
            }
            if let Err(e) = conn.process_new_packets() {
                log::error!("[LlmHandler {}] TLS error: {}", ctx.tree_id, e);
                send_llm_error(ctx, self.req_id, &format!("TLS error: {e}"));
                return false;
            }
        }
        true
    }

    fn feed_sse_bytes(&mut self, ctx: &mut WorkerCtx, data: &[u8]) -> bool {
        for ch in String::from_utf8_lossy(data).chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buf);
                if !line.is_empty() && !process_sse_line(ctx, self.req_id, &line) {
                    return false;
                }
            } else {
                self.line_buf.push(ch);
            }
        }
        true
    }
}

impl PollHandler for LlmHandler {
    fn fd(&self) -> RawFd {
        self.transport.fd()
    }

    fn interests(&self) -> PollFlags {
        match &self.transport {
            LlmTransport::Plain(_) => match &self.state {
                LlmState::SendRequest { body, sent } if *sent < body.len() => PollFlags::POLLOUT,
                _ => PollFlags::POLLIN,
            },
            LlmTransport::Tls { conn, .. } => {
                let mut flags = PollFlags::empty();
                if conn.wants_read() {
                    flags |= PollFlags::POLLIN;
                }
                if conn.wants_write() {
                    flags |= PollFlags::POLLOUT;
                }
                flags
            }
        }
    }

    fn on_ready(&mut self, ctx: &mut WorkerCtx) -> bool {
        let state_desc = match &self.state {
            LlmState::SendRequest { body, sent } => format!("SendRequest({sent}/{})", body.len()),
            LlmState::ReadHeaders { buf } => format!("ReadHeaders({})", buf.len()),
            LlmState::Streaming => format!("Streaming(line_buf={})", self.line_buf.len()),
        };
        log::debug!("[LlmHandler {}] on_ready state={}", ctx.tree_id, state_desc);

        if !self.do_tls_io(ctx) {
            return false;
        }

        match &mut self.state {
            LlmState::SendRequest { body, sent } => {
                if *sent >= body.len() {
                    self.state = LlmState::ReadHeaders { buf: Vec::new() };
                    return true;
                }
                match self.transport.write(&body[*sent..]) {
                    Ok(0) => {
                        send_llm_error(ctx, self.req_id, "connection closed during write");
                        return false;
                    }
                    Ok(n) => {
                        *sent += n;
                        log::debug!(
                            "[LlmHandler {}] wrote {} bytes ({}/{})",
                            ctx.tree_id,
                            n,
                            sent,
                            body.len()
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        send_llm_error(ctx, self.req_id, &format!("write error: {e}"));
                        return false;
                    }
                }
                if *sent >= body.len() {
                    self.state = LlmState::ReadHeaders { buf: Vec::new() };
                }
                true
            }
            LlmState::ReadHeaders { buf } => {
                let mut tmp = [0u8; 4096];
                loop {
                    match self.transport.read(&mut tmp) {
                        Ok(0) => {
                            send_llm_error(ctx, self.req_id, "connection closed before headers");
                            return false;
                        }
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            send_llm_error(ctx, self.req_id, &format!("read error: {e}"));
                            return false;
                        }
                    }
                }
                let mut hs = [httparse::EMPTY_HEADER; 32];
                let mut resp = httparse::Response::new(&mut hs);
                match resp.parse(buf) {
                    Ok(httparse::Status::Complete(hdr_end)) => {
                        let status = resp.code.unwrap_or(0);
                        if status != 200 {
                            let reason = resp.reason.unwrap_or("unknown");
                            let snippet = String::from_utf8_lossy(&buf[hdr_end..])
                                .chars()
                                .take(200)
                                .collect::<String>();
                            send_llm_error(
                                ctx,
                                self.req_id,
                                &format!("HTTP {status} {reason}: {snippet}"),
                            );
                            return false;
                        }
                        let trailing = buf[hdr_end..].to_vec();
                        if !self.feed_sse_bytes(ctx, &trailing) {
                            return false;
                        }
                        self.state = LlmState::Streaming;
                    }
                    Ok(httparse::Status::Partial) => {}
                    Err(_) => {
                        send_llm_error(ctx, self.req_id, "failed to parse HTTP response");
                        return false;
                    }
                }
                true
            }
            LlmState::Streaming => {
                let mut tmp = [0u8; 4096];
                loop {
                    match self.transport.read(&mut tmp) {
                        Ok(0) => {
                            if !self.line_buf.is_empty() {
                                process_sse_line(ctx, self.req_id, &self.line_buf);
                            }
                            send_llm_done(ctx, self.req_id);
                            return false;
                        }
                        Ok(n) => {
                            if !self.feed_sse_bytes(ctx, &tmp[..n]) {
                                return false;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            if !self.line_buf.is_empty() {
                                process_sse_line(ctx, self.req_id, &self.line_buf);
                            }
                            send_llm_error(ctx, self.req_id, &format!("stream error: {e}"));
                            return false;
                        }
                    }
                }
                true
            }
        }
    }
}

impl Drop for LlmHandler {
    fn drop(&mut self) {
        if let LlmTransport::Tls { tcp, conn } = &mut self.transport {
            conn.send_close_notify();
            let _ = conn.write_tls(tcp);
        }
    }
}

// ── URL parsing ──

fn parse_host_port_path(base_url: &str) -> Result<(String, u16, String), String> {
    let https = base_url.starts_with("https://");
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let (host_port, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{}", p.trim_end_matches('/'))),
        None => (rest, String::new()),
    };
    if let Some((host, port_str)) = host_port.rsplit_once(':') {
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("bad port in base_url: {base_url}"))?;
        Ok((host.to_string(), port, path))
    } else {
        let port = if https { 443 } else { 80 };
        Ok((host_port.to_string(), port, path))
    }
}

// ── SSE helpers ──

fn process_sse_line(ctx: &mut WorkerCtx, req_id: u64, line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed == ":" {
        return true;
    }
    if trimmed == "data: [DONE]" {
        log::debug!(
            "[worker_loop {}] SSE [DONE] -> sending Llm::Done",
            ctx.tree_id
        );
        send_llm_done(ctx, req_id);
        return false;
    }
    let data = if let Some(d) = trimmed.strip_prefix("data: ") {
        d
    } else {
        trimmed
    };
    log::debug!(
        "[worker_loop {}] SSE chunk -> worker: {}",
        ctx.tree_id,
        data.chars().take(120).collect::<String>()
    );
    ctx.send_pipe_in(&PipeIn::Llm(LlmResponse::Chunk {
        id: req_id,
        data: format!("{}\n", data),
    }));
    true
}

pub fn send_llm_error(ctx: &mut WorkerCtx, req_id: u64, message: &str) {
    log::warn!(
        "[worker_loop {}] LlmHandler error for req {}: {}",
        ctx.tree_id,
        req_id,
        message
    );
    ctx.send_pipe_in(&PipeIn::Llm(LlmResponse::Error {
        id: req_id,
        message: message.to_string(),
    }));
}

pub fn send_llm_done(ctx: &mut WorkerCtx, req_id: u64) {
    log::debug!(
        "[worker_loop {}] sending Llm::Done for req {}",
        ctx.tree_id,
        req_id
    );
    ctx.send_pipe_in(&PipeIn::Llm(LlmResponse::Done { id: req_id }));
}

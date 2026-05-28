use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::sync::Arc;

use agent_core::config::Config;
use agent_core::rpc::{LlmRequest, LlmResponse, PipeIn};
use serde_json::json;

use crate::provider::{self, Provider, StreamEvent};
use nix::poll::PollFlags;
use rustls::pki_types::ServerName;

use crate::worker_ctx::{PollHandler, WorkerCtx};

#[derive(Debug, thiserror::Error)]
pub enum LlmHandlerError {
    #[error("bad base URL: {0}")]
    BadUrl(String),
    #[error("connect to {addr}: {source}")]
    Connect { addr: String, #[source] source: std::io::Error },
    #[error("set nonblocking: {0}")]
    NonBlocking(#[source] std::io::Error),
    #[error("bad hostname: {0}")]
    BadHostname(String),
    #[error("rustls connect: {0}")]
    Rustls(#[source] rustls::Error),
    #[error("json body: {0}")]
    Json(#[from] serde_json::Error),
}

pub type LlmHandlerResult<T> = std::result::Result<T, LlmHandlerError>;

// ── Transport ──

#[allow(clippy::large_enum_variant)]
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

#[derive(Copy, Clone)]
enum ChunkDecode {
    Size,
    Data(usize),
    Trailer,
}

// ── LlmHandler ──

pub struct LlmHandler {
    transport: LlmTransport,
    state: LlmState,
    req_id: u64,
    line_buf: String,
    is_chunked: bool,
    chunk_decode: ChunkDecode,
    chunk_size_buf: Vec<u8>,
    provider: Box<dyn Provider>,
    got_done: bool,
}

impl LlmHandler {
    pub fn new(
        req: LlmRequest,
        cfg: &Config,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> LlmHandlerResult<Self> {
        let base_url = cfg.provider.base_url.trim_end_matches('/');
        let (host, port, base_path) = parse_host_port_path(base_url)?;
        let addr = format!("{}:{}", host, port);
        let is_tls = base_url.starts_with("https://");

        let prov = provider::create_provider(
            &cfg.provider.kind,
            &cfg.provider.base_url,
            &cfg.provider.api_key,
            &cfg.provider.model,
            cfg.provider.enable_thinking,
            &cfg.provider.reasoning_effort,
            cfg.provider.max_tokens,
            cfg.provider.sort.clone(),
        );
        let path = format!("{}{}", base_path, prov.endpoint_path());

        let tcp = TcpStream::connect(&addr)
            .map_err(|e| LlmHandlerError::Connect { addr: addr.clone(), source: e })?;
        tcp.set_nonblocking(true)
            .map_err(LlmHandlerError::NonBlocking)?;

        let transport = if is_tls {
            let server_name =
                ServerName::try_from(host.clone())
                    .map_err(|e| LlmHandlerError::BadHostname(format!("{e}")))?;
            let conn = rustls::ClientConnection::new(tls_config, server_name)
                .map_err(LlmHandlerError::Rustls)?;
            LlmTransport::Tls { tcp, conn }
        } else {
            LlmTransport::Plain(tcp)
        };

        let mut body = prov.build_body(&req.messages, &req.tools, true);
        // Set user for routing affinity so cache is consistent across requests.
        if let Some(ref rid) = req.routing_id {
            body["user"] = json!(rid);
        }
        let body_str = serde_json::to_string(&body)?;
        let auth_lines = prov.auth_header_lines();
        let request = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Content-Type: application/json\r\n\
             {}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            auth_lines,
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
            is_chunked: false,
            chunk_decode: ChunkDecode::Size,
            chunk_size_buf: Vec::new(),
            provider: prov,
            got_done: false,
        })
    }

    fn do_tls_io(&mut self, ctx: &mut WorkerCtx) -> bool {
        if let LlmTransport::Tls { tcp, conn } = &mut self.transport {
            if conn.wants_read() {
                match conn.read_tls(tcp) {
                    Ok(n) => {
                        log::debug!("[LlmHandler {}] TLS read_tls={}", ctx.tree_id, n);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        log::error!("[LlmHandler {}] TLS read_tls error: {}", ctx.tree_id, e);
                        send_llm_error(ctx, self.req_id, &format!("TLS read error: {e}"));
                        return false;
                    }
                }
            }
            if let Err(e) = conn.process_new_packets() {
                log::error!("[LlmHandler {}] TLS error: {}", ctx.tree_id, e);
                send_llm_error(ctx, self.req_id, &format!("TLS error: {e}"));
                return false;
            }
            if conn.wants_write() {
                let n = conn.write_tls(tcp).unwrap_or(0);
                log::debug!("[LlmHandler {}] TLS write_tls={}", ctx.tree_id, n);
            }
        }
        true
    }

    fn feed_sse_bytes(&mut self, ctx: &mut WorkerCtx, data: &[u8]) -> bool {
        for ch in String::from_utf8_lossy(data).chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buf);
                if !line.is_empty() && !self.handle_sse_line(ctx, &line) {
                    return false;
                }
            } else {
                self.line_buf.push(ch);
            }
        }
        true
    }

    fn handle_sse_line(&mut self, ctx: &mut WorkerCtx, line: &str) -> bool {
        match self.provider.parse_stream_event(line) {
            StreamEvent::Chunk(chunk) => {
                let data = serde_json::to_string(&chunk).unwrap_or_default();
                log::debug!(
                    "[worker_loop {}] sending ChatChunk -> worker: {}",
                    ctx.tree_id,
                    data.chars().take(500).collect::<String>()
                );
                ctx.send_pipe_in(&PipeIn::Llm(LlmResponse::Chunk {
                    id: self.req_id,
                    data,
                }));
                true
            }
            StreamEvent::Done => {
                log::debug!(
                    "[worker_loop {}] SSE done (deferred)",
                    ctx.tree_id
                );
                self.got_done = true;
                true // keep reading — usage data may follow [DONE]
            }
            StreamEvent::Skip => true,
        }
    }

    fn feed_bytes(&mut self, ctx: &mut WorkerCtx, data: &[u8]) -> bool {
        if !self.is_chunked {
            return self.feed_sse_bytes(ctx, data);
        }
        let mut i = 0;
        while i < data.len() {
            match self.chunk_decode {
                ChunkDecode::Size => {
                    let b = data[i];
                    i += 1;
                    if b == b'\n' {
                        let size_str = std::str::from_utf8(&self.chunk_size_buf).unwrap_or("0");
                        let hex = size_str.split(';').next().unwrap_or("0").trim();
                        let size = usize::from_str_radix(hex, 16).unwrap_or(0);
                        self.chunk_size_buf.clear();
                        if size == 0 {
                            if !self.line_buf.is_empty() {
                                let line = std::mem::take(&mut self.line_buf);
                                self.handle_sse_line(ctx, &line);
                            }
                            send_llm_done(ctx, self.req_id);
                            return false;
                        }
                        self.chunk_decode = ChunkDecode::Data(size);
                    } else if b != b'\r' {
                        self.chunk_size_buf.push(b);
                    }
                }
                ChunkDecode::Data(remaining) => {
                    let to_read = (data.len() - i).min(remaining);
                    let end = i + to_read;
                    let new_remaining = remaining - to_read;
                    self.chunk_decode = if new_remaining == 0 {
                        ChunkDecode::Trailer
                    } else {
                        ChunkDecode::Data(new_remaining)
                    };
                    if !self.feed_sse_bytes(ctx, &data[i..end]) {
                        return false;
                    }
                    i = end;
                }
                ChunkDecode::Trailer => {
                    if data[i] == b'\n' {
                        self.chunk_decode = ChunkDecode::Size;
                    }
                    i += 1;
                }
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
                        self.is_chunked = resp.headers.iter().any(|h| {
                            h.name.eq_ignore_ascii_case("transfer-encoding")
                                && String::from_utf8_lossy(h.value)
                                    .to_ascii_lowercase()
                                    .contains("chunked")
                        });
                        let trailing = buf[hdr_end..].to_vec();
                        if !self.feed_bytes(ctx, &trailing) {
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
                                let line = std::mem::take(&mut self.line_buf);
                                self.handle_sse_line(ctx, &line);
                            }
                            send_llm_done(ctx, self.req_id);
                            return false;
                        }
                        Ok(n) => {
                            if !self.feed_bytes(ctx, &tmp[..n]) {
                                // feed_bytes only returns false on error;
                                // [DONE] no longer stops processing via handle_sse_line.
                                // If the connection closed mid-parse, handle above.
                            }
                            if self.got_done {
                                // All available data has been consumed — send Done now.
                                send_llm_done(ctx, self.req_id);
                                return false;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if self.got_done {
                                send_llm_done(ctx, self.req_id);
                                return false;
                            }
                            break;
                        }
                        Err(e) => {
                            if !self.line_buf.is_empty() {
                                let line = std::mem::take(&mut self.line_buf);
                                self.handle_sse_line(ctx, &line);
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

fn parse_host_port_path(base_url: &str) -> LlmHandlerResult<(String, u16, String)> {
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
            .map_err(|_| LlmHandlerError::BadUrl(format!("bad port in base_url: {base_url}")))?;
        Ok((host.to_string(), port, path))
    } else {
        let port = if https { 443 } else { 80 };
        Ok((host_port.to_string(), port, path))
    }
}

// ── Helpers ──

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
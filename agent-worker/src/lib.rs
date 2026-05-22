pub mod agent;
mod context_files;
pub mod lsp_client;
mod thinking;
mod tools;
mod turn;
mod util;

use std::io::BufWriter;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::warn;

use agent_core::rpc::{LlmResponse, PipeIn, WsCommand};
use agent_core::store::Store;
use agent_core::types::{LspConfig, Message};

use agent_core::types::NotificationLevel;
use crate::turn::{begin_turn, cancel_turn, finish_response, process_chunk, resolve_lsp_wait_into, resolve_lsp_wait_with_timeout};
use crate::util::{emit_notification, parse_tree_id, read_config, resolve_repo_path};

pub(crate) enum AgentState {
    Idle,
    Streaming {
        messages: Vec<Message>,
        leaf_id: Option<String>,
        response_text: String,
        in_thinking: bool,
        tool_calls_buf: Vec<agent_core::types::ToolCallBuilder>,
        finish_reason: Option<agent_core::types::StopReason>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
        lsp_wait: Option<crate::lsp_client::LspWaitState>,
    },
}

impl AgentState {
    pub(crate) fn new_streaming(
        messages: Vec<Message>,
        leaf_id: Option<String>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
    ) -> Self {
        AgentState::Streaming {
            messages,
            leaf_id,
            response_text: String::new(),
            in_thinking: false,
            tool_calls_buf: vec![],
            finish_reason: None,
            tool_call_round,
            tool_calls_this_turn,
            consecutive_failures,
            lsp_wait: None,
        }
    }
}

/// Read from stdin fd into buf. Returns false on EOF.
fn read_stdin_into_buf(fd: std::os::fd::RawFd, buf: &mut Vec<u8>) -> bool {
    let mut tmp = [0u8; 65536];
    loop {
        match nix::unistd::read(fd, &mut tmp) {
            Ok(0) => return false,
            Ok(n) => buf.extend(&tmp[..n]),
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => break,
            Err(e) => {
                warn!("stdin read error: {}", e);
                break;
            }
        }
    }
    true
}

/// Parse complete newline-delimited messages from buf without reading from fd.
fn parse_pipe_messages(buf: &mut Vec<u8>) -> Vec<PipeIn> {
    let mut msgs = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(&buf[..pos]).to_string();
        buf.drain(..=pos);
        if !line.is_empty() {
            match serde_json::from_str(line.trim_end()) {
                Ok(msg) => msgs.push(msg),
                Err(e) => warn!("Ignoring unparseable PipeIn: {} — line: {}", e, line),
            }
        }
    }
    msgs
}

fn dispatch_pipe_in(
    msg: PipeIn,
    state: &mut AgentState,
    tree_id: &str,
    store: &Store,
    session_cfg: &agent_core::config::SessionConfig,
    tools: &[Box<dyn crate::tools::Tool>],
    ctx: &mut crate::tools::ToolContext,
    out: &mut BufWriter<std::io::Stdout>,
    lsp_cfg: &LspConfig,
) {
    match msg {
        PipeIn::Cmd(WsCommand::Message { params }) => {
            if matches!(state, AgentState::Idle) {
                *state = begin_turn(
                    params.text, tree_id, store, session_cfg,
                    tools, ctx, out,
                );
            }
        }
        PipeIn::Cmd(WsCommand::Stop) => {
            if matches!(state, AgentState::Streaming { .. }) {
                *state = cancel_turn(std::mem::replace(state, AgentState::Idle), tree_id, store, ctx, out);
            } else {
                ctx.stop.store(false, Ordering::Relaxed);
            }
        }
        PipeIn::Llm(LlmResponse::Chunk { data, .. }) => {
            if let AgentState::Streaming { .. } = state {
                process_chunk(&data, state, out);
            }
        }
        PipeIn::Llm(LlmResponse::Done { .. }) => {
            if matches!(state, AgentState::Streaming { .. }) {
                let old = std::mem::replace(state, AgentState::Idle);
                *state = finish_response(
                    old, tree_id, store, session_cfg,
                    tools, ctx, out, lsp_cfg,
                );
            }
        }
        PipeIn::Llm(LlmResponse::Error { message, .. }) => {
            if matches!(state, AgentState::Streaming { .. }) {
                emit_notification(out, NotificationLevel::Fatal, message);
                *state = AgentState::Idle;
            }
        }
        PipeIn::Config(_) => {}
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }));

    let tree_id = parse_tree_id()?;

    let mut reader = std::io::BufReader::new(std::io::stdin());
    let mut line = String::new();
    let config = read_config(&mut reader, &mut line)?;

    // BufReader may have pre-fetched bytes beyond the config line; save them before dropping.
    let mut stdin_buf: Vec<u8> = Vec::new();
    stdin_buf.extend_from_slice(reader.buffer());
    drop(reader);

    let log_file = config.logging_to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging_level, config.logging_to_stderr);
    let store = Store::default();
    let session_cfg = agent_core::config::SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };
    let cwd = resolve_repo_path(&store, &tree_id);
    let tools = tools::all_tools();
    let mut ctx = tools::ToolContext::new(cwd.clone());

    let mut out = BufWriter::new(std::io::stdout());

    let mut state = AgentState::Idle;

    // Switch stdin to non-blocking for the poll loop
    let stdin_fd = std::io::stdin().as_raw_fd();
    nix::fcntl::fcntl(
        stdin_fd,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    ).map_err(|e| format!("stdin set_nonblock: {e}"))?;

    loop {
        // Always process any buffered stdin data before polling — this handles
        // bytes the BufReader pre-fetched as well as data read in previous iterations.
        let msgs = parse_pipe_messages(&mut stdin_buf);
        for msg in msgs {
            dispatch_pipe_in(
                msg, &mut state, &tree_id, &store,
                &session_cfg, &tools, &mut ctx, &mut out,
                &config.lsp,
            );
        }

        let timeout = match &state {
            AgentState::Streaming { lsp_wait: Some(wait), .. } => {
                let until = wait.silence_until.min(wait.deadline);
                let ms = until.saturating_duration_since(Instant::now()).as_millis();
                nix::poll::PollTimeout::from(std::cmp::min(ms, u16::MAX as u128) as u16)
            }
            _ => nix::poll::PollTimeout::NONE,
        };

        let mut pollfds: Vec<nix::poll::PollFd> = std::iter::once(
            nix::poll::PollFd::new(
                unsafe { BorrowedFd::borrow_raw(stdin_fd) },
                nix::poll::PollFlags::POLLIN,
            )
        ).chain(
            ctx.lsp_clients.values().map(|c|
                nix::poll::PollFd::new(
                    unsafe { BorrowedFd::borrow_raw(c.stdout_fd) },
                    nix::poll::PollFlags::POLLIN,
                )
            )
        ).collect();

        nix::poll::poll(&mut pollfds, timeout).ok();

        // Read more stdin data if available; break on EOF.
        let stdin_flags = pollfds.first()
            .and_then(|p| p.revents())
            .unwrap_or(nix::poll::PollFlags::empty());
        if stdin_flags.intersects(nix::poll::PollFlags::POLLIN | nix::poll::PollFlags::POLLHUP) {
            let alive = read_stdin_into_buf(stdin_fd, &mut stdin_buf);
            if !alive {
                // Drain any remaining complete messages before exiting.
                let msgs = parse_pipe_messages(&mut stdin_buf);
                for msg in msgs {
                    dispatch_pipe_in(
                        msg, &mut state, &tree_id, &store,
                        &session_cfg, &tools, &mut ctx, &mut out,
                        &config.lsp,
                    );
                }
                break;
            }
        }

        // Drain LSP fds
        if !ctx.lsp_clients.is_empty() {
            let lang_ids: Vec<String> = ctx.lsp_clients.keys().cloned().collect();
            for (i, lang_id) in lang_ids.iter().enumerate() {
                let poll_idx = i + 1;
                if poll_idx >= pollfds.len() {
                    break;
                }
                if pollfds[poll_idx].revents().map_or(false, |r| r.contains(nix::poll::PollFlags::POLLIN)) {
                    let updated = ctx.lsp_clients.get_mut(lang_id).unwrap().read_available();
                    if updated {
                        if let AgentState::Streaming { lsp_wait: Some(ref mut wait), ref mut messages, .. } = state {
                            wait.silence_until = Instant::now() + Duration::from_millis(wait.silence_ms);
                            let mut resolved_indices: Vec<usize> = Vec::new();
                            let mut resolved_responses: Vec<(serde_json::Value, String, String)> = Vec::new();
                            for (j, pending) in wait.pending_tool_requests.iter().enumerate() {
                                if let Some(client) = ctx.lsp_clients.get_mut(&pending.lang_id) {
                                    if let Some(response) = client.pending_responses.remove(&pending.request_id) {
                                        resolved_indices.push(j);
                                        resolved_responses.push((
                                            response,
                                            pending.tool_name.clone(),
                                            pending.tool_call_id.clone(),
                                        ));
                                    }
                                }
                            }
                            for (response, tool_name, tool_call_id) in resolved_responses {
                                if let Some(tool) = tools.iter().find(|t| t.name() == tool_name) {
                                    let result = tool.resume(response, &mut ctx);
                                    messages.push(crate::turn::make_tool_result_message(&tool_call_id, &tool_name, &result));
                                }
                            }
                            for j in resolved_indices.into_iter().rev() {
                                wait.pending_tool_requests.swap_remove(j);
                            }
                        }
                    }
                }
            }
        }

        // Resolve LSP wait if ready
        if let AgentState::Streaming { lsp_wait: Some(ref wait), .. } = state {
            let now = Instant::now();
            let tools_done = wait.pending_tool_requests.is_empty();
            if tools_done && (now >= wait.silence_until || now >= wait.deadline) {
                let old = std::mem::replace(&mut state, AgentState::Idle);
                state = resolve_lsp_wait_into(old, &ctx.lsp_clients, &mut out, &tools);
            } else if !tools_done && now >= wait.deadline {
                let old = std::mem::replace(&mut state, AgentState::Idle);
                state = resolve_lsp_wait_with_timeout(old, &mut ctx, &mut out, &tools);
            }
        }

        // Re-poll immediately if lsp_wait was just resolved to a new Streaming state.
        if matches!(state, AgentState::Streaming { lsp_wait: Some(_), .. }) {
            continue;
        }
    }
    Ok(())
}

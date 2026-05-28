pub mod agent;
mod context_files;
pub mod lsp_client;
pub mod store;
mod thinking;
mod tools;
mod turn;
mod util;

use std::io::{BufWriter, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

use log::warn;

use agent_core::rpc::{LlmResponse, PipeIn, WsCommand};
use crate::store::Store;
use agent_core::types::{Entry, LspConfig, Message, NotificationLevel, ServerEvent, SessionStatus, TreeMeta};
use crate::turn::{begin_turn, cancel_turn, finish_response, process_chunk, resolve_lsp_wait_into, resolve_lsp_wait_with_timeout};
use crate::util::{emit_notification, parse_tree_id, read_config, resolve_repo_path, WorkerError, WorkerResult};

pub(crate) enum AgentState {
    Idle,
    Streaming {
        messages: Vec<Message>,
        leaf_id: Option<String>,
        response_text: String,
        thinking_text: String,
        in_thinking: bool,
        saw_reasoning_field: bool,
        thinking_phase_done: bool,
        tool_calls_buf: Vec<agent_core::types::ToolCallBuilder>,
        finish_reason: Option<agent_core::types::StopReason>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
        lsp_wait: Option<crate::lsp_client::LspWaitState>,
        cum_prompt_tokens: u64,
        cum_completion_tokens: u64,
        cum_cached_tokens: u64,
        seen_cached_tokens: bool,
    },
    AutoTitling {
        accumulated: String,
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
            thinking_text: String::new(),
            in_thinking: false,
            saw_reasoning_field: false,
            thinking_phase_done: false,
            tool_calls_buf: vec![],
            finish_reason: None,
            tool_call_round,
            tool_calls_this_turn,
            consecutive_failures,
            lsp_wait: None,
            cum_prompt_tokens: 0,
            cum_completion_tokens: 0,
            cum_cached_tokens: 0,
            seen_cached_tokens: false,
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
    store: &Store,
    session_cfg: &agent_core::config::SessionConfig,
    tools: &[Box<dyn crate::tools::Tool>],
    ctx: &mut crate::tools::ToolContext,
    out: &mut BufWriter<std::io::Stdout>,
    lsp_cfg: &LspConfig,
    req_id: &mut u64,
) {
    match msg {
        PipeIn::Cmd(WsCommand::Message { params }) => {
            match state {
                AgentState::Idle => {
                    *state = begin_turn(
                        params.text, store, session_cfg,
                        tools, ctx, out, req_id,
                    );
                }
                AgentState::AutoTitling { .. } => {
                    log::warn!("[worker] dropping message while auto-titling: {}", params.text);
                }
                _ => {}
            }
        }
        PipeIn::Cmd(WsCommand::Stop) => {
            if matches!(state, AgentState::Streaming { .. }) {
                *state = cancel_turn(std::mem::replace(state, AgentState::Idle), store, ctx, out);
            } else {
                ctx.stop.store(false, Ordering::Relaxed);
            }
        }
        PipeIn::Cmd(WsCommand::GetEntries { count }) => {
            let entries = store.read_all_entries().unwrap_or_default();
            let to_emit: &[Entry] = if let Some(n) = count {
                let len = entries.len();
                &entries[len.saturating_sub(n)..]
            } else {
                &entries
            };
            for entry in to_emit {
                crate::util::emit_event(out, ServerEvent::Entry(entry.clone()));
            }
            crate::util::emit_event(out, ServerEvent::Done { status: "history".into(), usage: None });
            out.flush().ok();
        }
        PipeIn::Cmd(WsCommand::AutoTitle) => {
            if !matches!(state, AgentState::Idle) { return; }
            let meta = match store.get_tree().ok() {
                Some(m) => m,
                None => return,
            };
            if meta.title.is_some() { return; }
            let entries = store.read_all_entries().unwrap_or_default();
            let leaf_id = match &meta.leaf_id {
                Some(id) => id.clone(),
                None => return,
            };
            let mut messages = crate::agent::build_context(&entries, &leaf_id);
            messages.insert(0, Message {
                role: agent_core::types::MessageRole::System,
                content: agent_core::types::MessageContent::Text(
                    "Generate a concise title (6 words or fewer) for this coding \
                     conversation. Return ONLY the title text, no quotes, no \
                     punctuation, no explanation.".into()
                ),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
                thinking: None,
            });
            *req_id += 1;
            let llm_req = agent_core::rpc::LlmRequest { id: *req_id, messages, tools: vec![], routing_id: None };
            agent_core::rpc::write_json_line(out, &agent_core::rpc::PipeOut::Llm(llm_req))
                .ok();
            out.flush().ok();
            *state = AgentState::AutoTitling { accumulated: String::new() };
        }
        PipeIn::Llm(LlmResponse::Chunk { id, data, .. }) => {
            if id != *req_id {
                return;
            }
            match state {
                AgentState::Streaming { .. } => {
                    process_chunk(&data, state, out);
                }
                AgentState::AutoTitling { ref mut accumulated, .. } => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                        if let Some(t) = v["delta_text"].as_str() {
                            accumulated.push_str(t);
                        }
                    }
                }
                _ => {}
            }
        }
        PipeIn::Llm(LlmResponse::Done { id, .. }) => {
            if id != *req_id {
                return;
            }
            match state {
                AgentState::Streaming { .. } => {
                    let old = std::mem::replace(state, AgentState::Idle);
                    *state = finish_response(
                        old, store, session_cfg,
                        tools, ctx, out, lsp_cfg, req_id,
                    );
                }
                AgentState::AutoTitling { ref accumulated, .. } => {
                    let title = accumulated.trim().trim_matches('"').to_string();
                    if !title.is_empty() {
                        if let Ok(mut meta) = store.get_tree() {
                            meta.title = Some(title.clone());
                            let _ = store.save_tree_meta(&meta);
                        }
                        crate::util::emit_event(out, ServerEvent::MetaUpdate { title: Some(title), model: None });
                        out.flush().ok();
                    }
                    *state = AgentState::Idle;
                }
                _ => {}
            }
        }
        PipeIn::Llm(LlmResponse::Error { id, message, .. }) => {
            if id != *req_id {
                return;
            }
            match state {
                AgentState::Streaming { .. } => {
                    emit_notification(out, NotificationLevel::Fatal, message);
                    *state = AgentState::Idle;
                }
                AgentState::AutoTitling { .. } => {
                    log::warn!("[worker] auto-title LLM error: {}", message);
                    *state = AgentState::Idle;
                }
                _ => {}
            }
        }
        PipeIn::Config(_) => {}
    }
}

fn startup_writes(store: &Store) -> Result<(), WorkerError> {
    let tree_id = store.tree_id();
    let entries = store.read_all_entries().unwrap_or_default();

    // If entries exist and the last entry is not a SessionEnd, write a recovery
    // SessionEnd to mark the previous session as aborted.
    let recovery_parent = if !entries.is_empty() {
        match entries.last() {
            Some(Entry::SessionEnd { .. }) => None,
            _ => {
                let meta = store.get_tree().ok();
                let parent_id = meta.as_ref().and_then(|m| m.leaf_id.clone());
                let entry = Entry::SessionEnd {
                    id: agent_core::util::generate_entry_id(),
                    parent_id: parent_id.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    summary: Some("session aborted (worker exit or server shutdown)".into()),
                    status: SessionStatus::Aborted,
                    continuation_brief: None,
                };
                let _ = store.append_entry(&entry);
                log::info!(
                    "[worker] startup: tree={} recovery session_end appended (parent={:?})",
                    tree_id,
                    parent_id
                );
                Some(entry)
            }
        }
    } else {
        None
    };

    // Write a new SessionStart, linked to the previous entry so
    // build_context can walk the parent chain across sessions.
    let session_start_parent = match &recovery_parent {
        Some(recovery) => Some(recovery.id().to_string()),
        None => entries.last().map(|e| e.id().to_string()),
    };
    let session_start = Entry::SessionStart {
        id: agent_core::util::generate_entry_id(),
        parent_id: session_start_parent,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    store.append_entry(&session_start)?;

    // Update meta.leaf_id to point at the new SessionStart
    let mut meta: TreeMeta = store.get_tree()
        .unwrap_or_else(|_| TreeMeta {
            id: tree_id.to_string(),
            parent_id: None,
            repo_path: None,
            title: None,
            created_at: chrono::Utc::now().timestamp(),
            updated_at: chrono::Utc::now().timestamp(),
            leaf_id: None,
            sandbox: Default::default(),
        });
    meta.leaf_id = Some(session_start.id().to_string());
    meta.updated_at = chrono::Utc::now().timestamp();
    store.save_tree_meta(&meta)?;

    log::info!(
        "[worker] startup: tree={} recovery={} new session_start={}",
        tree_id,
        recovery_parent.is_some(),
        session_start.id()
    );

    Ok(())
}

pub fn run() -> WorkerResult<()> {
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

    ctrlc::set_handler(|| SIGTERM_RECEIVED.store(true, Ordering::Relaxed)).ok();
    let store = Store::new_default(&tree_id);
    startup_writes(&store)?;
    let session_cfg = agent_core::config::SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };
    let cwd = resolve_repo_path(&store);
    let tools = tools::all_tools();
    let mut ctx = tools::ToolContext::new(cwd.clone());

    let mut out = BufWriter::new(std::io::stdout());

    let mut state = AgentState::Idle;
    let mut req_id: u64 = 0;

    // Switch stdin to non-blocking for the poll loop
    let stdin_fd = std::io::stdin().as_raw_fd();
    nix::fcntl::fcntl(
        stdin_fd,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )?;

    loop {
        // Always process any buffered stdin data before polling — this handles
        // bytes the BufReader pre-fetched as well as data read in previous iterations.
        let msgs = parse_pipe_messages(&mut stdin_buf);
        for msg in msgs {
            dispatch_pipe_in(
                msg, &mut state, &store,
                &session_cfg, &tools, &mut ctx, &mut out,
                &config.lsp, &mut req_id,
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
            // SAFETY: stdin_fd is stdin's fd, valid for the lifetime of the process.
            nix::poll::PollFd::new(
                unsafe { BorrowedFd::borrow_raw(stdin_fd) },
                nix::poll::PollFlags::POLLIN,
            )
        ).chain(
            ctx.lsp_clients.values().map(|c|
                // SAFETY: c.stdout_fd is owned by LspClient which lives in ctx for the loop.
                nix::poll::PollFd::new(
                    unsafe { BorrowedFd::borrow_raw(c.stdout_fd) },
                    nix::poll::PollFlags::POLLIN,
                )
            )
        ).collect();

        nix::poll::poll(&mut pollfds, timeout).ok();

        if SIGTERM_RECEIVED.load(Ordering::Relaxed) {
            if matches!(state, AgentState::Streaming { .. }) {
                crate::util::emit_event(&mut out, agent_core::types::ServerEvent::Done { status: "aborted".into(), usage: None });
            }
            break;
        }

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
                        msg, &mut state, &store,
                        &session_cfg, &tools, &mut ctx, &mut out,
                        &config.lsp, &mut req_id,
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
                state = resolve_lsp_wait_into(old, &mut ctx.lsp_clients, &mut out, &tools, &mut req_id, Some(store.tree_id().to_string()));
            } else if !tools_done && now >= wait.deadline {
                let old = std::mem::replace(&mut state, AgentState::Idle);
                state = resolve_lsp_wait_with_timeout(old, &mut ctx, &mut out, &tools, &mut req_id, Some(store.tree_id().to_string()));
            }
        }

        // Re-poll immediately if lsp_wait was just resolved to a new Streaming state.
        if matches!(state, AgentState::Streaming { lsp_wait: Some(_), .. }) {
            continue;
        }
    }
    Ok(())
}

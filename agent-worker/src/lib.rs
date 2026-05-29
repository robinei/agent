pub mod agent;
mod context_files;
pub mod llm;
pub mod store;
mod thinking;
mod tools;
// mod turn; — replaced by inline run_turn in Phase 3; module removed in Step 3
mod util;

use std::cell::RefCell;
use std::io::{BufWriter, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;
use log::{error, info, warn};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolOutput};
use crate::util::{
    emit_event, emit_notification, parse_tree_id, read_config, resolve_repo_path, WorkerError,
    WorkerResult,
};
use agent_core::config::SessionConfig;
use agent_core::rpc::{LlmResponse, PipeIn, PipeOut, WsCommand};
use agent_core::types::{
    Entry, Message, MessageContent, MessageRole, NotificationLevel, ServerEvent,
    SessionStatus, StopReason, ToolCallBuilder, TreeMeta,
};
use agent_core::util::generate_entry_id;

pub(crate) static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

// ── TurnCtx: !Send state shared across turns ──

/// State that persists across turns. !Send because WorkerLlmClient holds
/// Rc<RefCell<...>> and ToolContext is not Send. This is fine: TurnCtx
/// lives inside the block_on loop (see TOKIO.md §Concurrency model).
pub(crate) struct TurnCtx {
    pub(crate) store: Store,
    pub(crate) tool_ctx: ToolContext,
    pub(crate) tools: Vec<Box<dyn Tool>>,
    pub(crate) pipe_tx: mpsc::Sender<PipeOut>,
    pub(crate) llm: crate::llm::WorkerLlmClient,
    pub(crate) session_cfg: SessionConfig,

}

// ── Parse PipeIn from buffer ──

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

// ── Stdin forwarder task ──

async fn stdin_forwarder(tx: mpsc::Sender<PipeIn>) {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<PipeIn>(trimmed) {
            Ok(msg) => {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
            Err(e) => warn!("Ignoring unparseable PipeIn: {} — line: {}", e, trimmed),
        }
    }
}

// ── Signal handler task ──

async fn signal_task(tx: mpsc::Sender<()>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
    let _ = tx.send(()).await;
    SIGTERM_RECEIVED.store(true, Ordering::Relaxed);
}

// ── Stdout writer task ──

async fn stdout_writer(mut rx: mpsc::Receiver<PipeOut>) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(std::io::stdout());
    while let Some(msg) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = writeln!(out, "{}", json);
            let _ = out.flush();
        }
    }
}

// ── Startup writes ──

async fn startup_writes(store: &Store) -> Result<(), WorkerError> {
    let tree_id = store.tree_id();
    let entries = store.read_all_entries().await.unwrap_or_default();

    let recovery_parent = if !entries.is_empty() {
        match entries.last() {
            Some(Entry::SessionEnd { .. }) => None,
            _ => {
                let meta = store.get_tree().await.ok();
                let parent_id = meta.as_ref().and_then(|m| m.leaf_id.clone());
                let entry = Entry::SessionEnd {
                    id: generate_entry_id(),
                    parent_id: parent_id.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    summary: Some("session aborted (worker exit or server shutdown)".into()),
                    status: SessionStatus::Aborted,
                    continuation_brief: None,
                };
                let _ = store.append_entry(&entry).await;
                info!(
                    "[worker] startup: tree={} recovery session_end appended (parent={:?})",
                    tree_id, parent_id
                );
                Some(entry)
            }
        }
    } else {
        None
    };

    let session_start_parent = match &recovery_parent {
        Some(recovery) => Some(recovery.id().to_string()),
        None => entries.last().map(|e| e.id().to_string()),
    };
    let session_start = Entry::SessionStart {
        id: generate_entry_id(),
        parent_id: session_start_parent,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    store.append_entry(&session_start).await?;

    let mut meta: TreeMeta = store.get_tree().await.unwrap_or_else(|_| TreeMeta {
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
    store.save_tree_meta(&meta).await?;

    info!(
        "[worker] startup: tree={} recovery={} new session_start={}",
        tree_id,
        recovery_parent.is_some(),
        session_start.id()
    );

    Ok(())
}

fn parse_exit_code(content: &str) -> i32 {
    for line in content.lines().rev() {
        if let Some(rest) = line.strip_prefix("exit_code: ") {
            if let Ok(code) = rest.trim().parse::<i32>() {
                return code;
            }
        }
    }
    0
}

// ── Run turn ──

async fn run_turn(
    text: String,
    turn_ctx: Rc<RefCell<TurnCtx>>,
    pipe_in_rx: &mut mpsc::Receiver<PipeIn>,
) -> WorkerResult<()> {
    let tree_id;
    let tool_defs: Vec<agent_core::types::ToolDefinition>;
    let max_per_turn;
    let context_window = 128_000usize;
    let soft_cap_pct;
    let hard_cap_pct;

    // Read minimal config from ctx
    {
        let ctx = turn_ctx.borrow();
        tree_id = ctx.store.tree_id().to_string();
        max_per_turn = ctx.session_cfg.max_tool_calls_per_turn;
        soft_cap_pct = ctx.session_cfg.soft_cap_pct;
        hard_cap_pct = ctx.session_cfg.hard_cap_pct;
        tool_defs = ctx.tools.iter().map(|t| t.definition()).collect();
    }

    // Read entries and build message context
    let entries;
    let leaf_id;
    let ctx_files;
    let cwd;
    let agent_dir;

    {
        let ctx = turn_ctx.borrow();
        entries = ctx.store.read_all_entries().await.map_err(|e| {
            WorkerError::Other(format!("Failed to read entries: {}", e))
        })?;
        let tree_meta = ctx.store.get_tree().await.map_err(|e| {
            WorkerError::Other(format!("Failed to get tree meta: {}", e))
        })?;
        leaf_id = tree_meta.leaf_id.clone();
        cwd = ctx.tool_ctx.cwd.clone();
        agent_dir = ctx.store.base_dir().clone();
    }
    // load_context_files is async; call it outside the borrow scope
    ctx_files = crate::context_files::load_context_files(&cwd, &agent_dir).await;

    let leaf_ref = leaf_id.as_deref().unwrap_or("root");
    let mut messages = crate::agent::build_context(&entries, leaf_ref);

    // Create user message
    let user_msg_id = generate_entry_id();
    let mut current_leaf = leaf_id.clone();

    let user_msg = Message {
        role: MessageRole::User,
        content: MessageContent::Text(text),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
        thinking: None,
    };

    // Store and emit user message
    {
        let ctx = turn_ctx.borrow();
        let user_entry = Entry::Message {
            id: user_msg_id.clone(),
            parent_id: current_leaf.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: user_msg.clone(),
        };
        let _ = ctx.store.append_entry(&user_entry).await;
        let _ = ctx
            .pipe_tx
            .send(PipeOut::Event(ServerEvent::Entry(user_entry)))
            .await;
    }
    current_leaf = Some(user_msg_id.clone());
    messages.push(user_msg);

    // Add system prompt
    let context_section = crate::context_files::format_context_section(&ctx_files);
    let system_prompt = format!(
        "You are a coding agent working in a repository. Always respond in English.\n\
         Repo path: {}\n\
         Version control: {}\n\
         When listing files, prefer `rg --files` over `find` — it respects .gitignore.\n\
         \n\
         {}",
        cwd.display(),
        if cwd.join(".git").exists() {
            "git"
        } else {
            "none — this is not a git repository, do not run git commands"
        },
        context_section
    );
    messages.insert(
        0,
        Message {
            role: MessageRole::System,
            content: MessageContent::Text(system_prompt),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        },
    );

    // Context cap check
    let estimated = crate::agent::estimate_context_tokens(&messages);
    let soft_cap = (soft_cap_pct as usize) * context_window / 100;
    let hard_cap = (hard_cap_pct as usize) * context_window / 100;
    let pct = (estimated * 100 / context_window) as u8;

    {
        let ctx = turn_ctx.borrow();
        let _ = ctx
            .pipe_tx
            .send(PipeOut::Event(ServerEvent::ContextUpdate {
                status: if estimated >= hard_cap {
                    agent_core::types::ContextStatus::Critical
                } else if estimated >= soft_cap {
                    agent_core::types::ContextStatus::Warning
                } else {
                    agent_core::types::ContextStatus::Ok
                },
                pct,
                estimated: estimated as u64,
            }))
            .await;

        if estimated >= hard_cap {
            warn!("Hard cap reached for tree {} (est. {} tokens)", tree_id, estimated);
            let _ = ctx
                .pipe_tx
                .send(PipeOut::Event(ServerEvent::Notification {
                    level: NotificationLevel::Error,
                    message: "Hard context cap reached. Ending session.".into(),
                }))
                .await;
            let session_end = Entry::SessionEnd {
                id: generate_entry_id(),
                parent_id: current_leaf.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                summary: None,
                status: SessionStatus::Continuing,
                continuation_brief: None,
            };
            let _ = ctx.store.append_entry(&session_end).await;
            let _ = ctx
                .pipe_tx
                .send(PipeOut::Event(ServerEvent::Entry(session_end)))
                .await;
            let _ = ctx
                .pipe_tx
                .send(PipeOut::Event(ServerEvent::Done {
                    status: "error".into(),
                    usage: None,
                }))
                .await;
            return Ok(());
        }
    }

    // ── Main LLM ↔ tool loop ──
    let mut tool_call_round = 0usize;
    let mut tool_calls_this_turn = 0usize;
    let mut consecutive_failures = 0usize;

    loop {
        // Start streaming LLM request
        let mut stream;
        {
            let ctx = turn_ctx.borrow();
            stream = ctx.llm.request(messages.clone(), tool_defs.clone(), Some(tree_id.clone()));
        }

        let mut response_text = String::new();
        let mut thinking_text = String::new();
        let mut in_thinking = false;
        let mut tool_calls_buf: Vec<ToolCallBuilder> = Vec::new();
        let mut finish_reason: Option<StopReason> = None;

        // Stream loop — drive the LLM response while also routing PipeIn::Llm
        loop {
            tokio::select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(text)) => {
                            if !text.is_empty() {
                                response_text.push_str(&text);
                                let ctx = turn_ctx.borrow();
                                let _ = ctx.pipe_tx
                                    .send(PipeOut::Event(ServerEvent::TextChunk { content: text }))
                                    .await;
                            }
                        }
                        Some(Err(e)) => {
                            warn!("LLM stream error during turn: {}", e);
                            let ctx = turn_ctx.borrow();
                            let _ = ctx.pipe_tx
                                .send(PipeOut::Event(ServerEvent::Notification {
                                    level: NotificationLevel::Error,
                                    message: format!("LLM error: {}", e),
                                }))
                                .await;
                            break;
                        }
                        None => break,
                    }
                }
                Some(pipe_in) = pipe_in_rx.recv() => {
                    match pipe_in {
                        PipeIn::Llm(resp) => {
                            let ctx = turn_ctx.borrow();
                            ctx.llm.route(resp);
                        }
                        PipeIn::Cmd(WsCommand::Stop) => {
                            info!("[worker] Turn cancelled by Stop");
                            let ctx = turn_ctx.borrow();
                            let _ = ctx.pipe_tx
                                .send(PipeOut::Event(ServerEvent::Done {
                                    status: "cancelled".into(),
                                    usage: None,
                                }))
                                .await;
                            return Ok(());
                        }
                        _ => {
                            // Non-LLM PipeIns during a turn are deferred to the
                            // main event loop. This is intentional — only Stop and
                            // Llm are handled mid-turn.
                            log::debug!("Received non-LLM PipeIn during turn, deferring");
                        }
                    }
                }
            }
        }

        // Collect final response
        let final_resp = stream.finish();

        // Peek at the builder for tool calls
        // LlmStream::finish() returns the ChatResponse which has tool_calls
        // Let's check finish_reason from the response
        finish_reason = match final_resp.finish_reason.as_str() {
            "ToolCalls" | "tool_calls" => Some(StopReason::ToolCalls),
            "length" => Some(StopReason::Length),
            _ => Some(StopReason::Stop),
        };

        // Write assistant message entry
        if !response_text.is_empty() || final_resp.tool_calls.is_some() {
            let msg_id = generate_entry_id();
            let thinking = if thinking_text.is_empty() {
                None
            } else {
                Some(thinking_text.clone())
            };
            let assistant_msg = Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text(response_text.clone()),
                tool_calls: final_resp.tool_calls.clone(),
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
                thinking,
            };
            let entry = Entry::Message {
                id: msg_id.clone(),
                parent_id: current_leaf.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                message: assistant_msg.clone(),
            };
            {
                let ctx = turn_ctx.borrow();
                let _ = ctx.store.append_entry(&entry).await;
                let _ = ctx
                    .pipe_tx
                    .send(PipeOut::Event(ServerEvent::Entry(entry)))
                    .await;
            }
            current_leaf = Some(msg_id);
            messages.push(assistant_msg);
        }

        match finish_reason {
            Some(StopReason::ToolCalls) => {
                let calls = match final_resp.tool_calls {
                    Some(c) => c,
                    None => break,
                };

                tool_call_round += 1;

                for call in &calls {
                    tool_calls_this_turn += 1;
                    if tool_calls_this_turn > max_per_turn {
                        warn!("Max tool calls per turn reached for tree {}", tree_id);
                        let ctx = turn_ctx.borrow();
                        let _ = ctx
                            .pipe_tx
                            .send(PipeOut::Event(ServerEvent::Notification {
                                level: NotificationLevel::Error,
                                message: format!("Max tool calls per turn ({}) reached", max_per_turn),
                            }))
                            .await;
                        let _ = ctx
                            .pipe_tx
                            .send(PipeOut::Event(ServerEvent::Done {
                                status: "error".into(),
                                usage: None,
                            }))
                            .await;
                        return Ok(());
                    }

                    // Get the tool_call_id — the tool needs it in the result message
                    let tool_call_id = call.id.clone();

                    // Emit ToolStart
                    {
                        let ctx = turn_ctx.borrow();
                        let _ = ctx
                            .pipe_tx
                            .send(PipeOut::Event(ServerEvent::ToolStart {
                                tool: call.name.clone(),
                                input: call.arguments.clone(),
                            }))
                            .await;
                    }

                    // Execute tool (borrow ctx mutably for tool_ctx access)
                    let result = {
                        let mut ctx_borrow = turn_ctx.borrow_mut();
                        let ctx = &mut *ctx_borrow;
                        let tool = ctx.tools.iter().find(|t| t.name() == call.name);

                        match tool {
                            Some(t) => t.execute(&call.arguments, &mut ctx.tool_ctx).await,
                            None => ToolOutput::Done(Err(format!("Unknown tool '{}'", call.name))),
                        }
                    };

                    match result {
                        ToolOutput::Done(res) => {
                            let is_error = res.is_err();
                            let content = match &res {
                                Ok(c) => c.clone(),
                                Err(e) => e.clone(),
                            };

                            if is_error {
                                consecutive_failures += 1;
                                if consecutive_failures >= 3 {
                                    warn!("3 consecutive tool failures for tree {}", tree_id);
                                    let ctx = turn_ctx.borrow();
                                    let _ = ctx.pipe_tx
                                        .send(PipeOut::Event(ServerEvent::Notification {
                                            level: NotificationLevel::Error,
                                            message: "3 consecutive tool failures, aborting turn".into(),
                                        }))
                                        .await;
                                    let _ = ctx.pipe_tx
                                        .send(PipeOut::Event(ServerEvent::Done {
                                            status: "error".into(),
                                            usage: None,
                                        }))
                                        .await;
                                    return Ok(());
                                }
                            } else {
                                consecutive_failures = 0;
                            }

                            // Emit ToolResult
                            let preview: String = content.chars().take(2000).collect();
                            let output = if content.len() > 2000 {
                                format!("{}... (truncated, was {} bytes)", preview, content.len())
                            } else {
                                content.clone()
                            };
                            {
                                let ctx = turn_ctx.borrow();
                                let _ = ctx
                                    .pipe_tx
                                    .send(PipeOut::Event(ServerEvent::ToolResult {
                                        tool: call.name.clone(),
                                        exit: if is_error { 1 } else { 0 },
                                        output,
                                    }))
                                    .await;
                            }

                            // Bash entry
                            if call.name == "bash" {
                                let exit_code = if is_error { 1 } else { parse_exit_code(&content) };
                                let command = call
                                    .arguments
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let bash_entry = Entry::BashExec {
                                    id: generate_entry_id(),
                                    parent_id: current_leaf.clone(),
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    command,
                                    output: content.clone(),
                                    exit_code,
                                    truncated: content.contains("Output truncated"),
                                    duration_ms: None,
                                };
                                let ctx = turn_ctx.borrow();
                                let _ = ctx.store.append_entry(&bash_entry).await;
                                let _ = ctx
                                    .pipe_tx
                                    .send(PipeOut::Event(ServerEvent::Entry(bash_entry)))
                                    .await;
                            }

                            // Make tool result message
                            let tool_result_msg = Message {
                                role: MessageRole::Tool,
                                content: MessageContent::Text(content),
                                tool_calls: None,
                                tool_call_id: Some(tool_call_id),
                                tool_name: Some(call.name.clone()),
                                usage: None,
                                stop_reason: None,
                                is_error: if is_error { Some(true) } else { None },
                                thinking: None,
                            };
                            let result_msg_id = generate_entry_id();
                            {
                                let ctx = turn_ctx.borrow();
                                let entry = Entry::Message {
                                    id: result_msg_id.clone(),
                                    parent_id: current_leaf.clone(),
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    message: tool_result_msg.clone(),
                                };
                                let _ = ctx.store.append_entry(&entry).await;
                                let _ = ctx
                                    .pipe_tx
                                    .send(PipeOut::Event(ServerEvent::Entry(entry)))
                                    .await;
                            }
                            current_leaf = Some(result_msg_id);
                            messages.push(tool_result_msg);
                        }
                        ToolOutput::PendingLsp { .. } => {
                            // LSP not implemented yet — skip
                        }
                    }
                }

                if consecutive_failures >= 3 || tool_call_round >= max_per_turn {
                    if tool_call_round >= max_per_turn {
                        let ctx = turn_ctx.borrow();
                        let _ = ctx.pipe_tx
                            .send(PipeOut::Event(ServerEvent::Notification {
                                level: NotificationLevel::Error,
                                message: format!("Max tool call rounds ({}) reached", max_per_turn),
                            }))
                            .await;
                        let _ = ctx.pipe_tx
                            .send(PipeOut::Event(ServerEvent::Done {
                                status: "error".into(),
                                usage: None,
                            }))
                            .await;
                    }
                    return Ok(());
                }

                // Loop back for next LLM round
                continue;
            }
            _ => {
                // Turn complete — emit Done
                let ctx = turn_ctx.borrow();
                let _ = ctx
                    .pipe_tx
                    .send(PipeOut::Event(ServerEvent::Done {
                        status: final_resp.finish_reason.clone(),
                        usage: None,
                    }))
                    .await;
                break;
            }
        }
    }

    Ok(())
}

// ── Auto-title ──

async fn auto_title(ctx: &TurnCtx) {
    let leaf_id = match ctx.store.get_tree().await.ok() {
        Some(m) => {
            if m.title.is_some() { return; }
            match m.leaf_id {
                Some(id) => id,
                None => return,
            }
        }
        None => return,
    };

    let entries = match ctx.store.read_all_entries().await {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut messages = crate::agent::build_context(&entries, &leaf_id);
    messages.insert(0, Message {
        role: MessageRole::System,
        content: MessageContent::Text(
            "Generate a concise title (6 words or fewer) for this coding \
             conversation. Return ONLY the title text, no quotes, no \
             punctuation, no explanation.".into(),
        ),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
        thinking: None,
    });

    match ctx.llm.complete(messages, vec![]).await {
        Ok(resp) => {
            let title = resp.text.trim().trim_matches('"').to_string();
            if !title.is_empty() {
                if let Ok(mut meta) = ctx.store.get_tree().await {
                    meta.title = Some(title.clone());
                    let _ = ctx.store.save_tree_meta(&meta).await;
                }
                let _ = ctx
                    .pipe_tx
                    .send(PipeOut::Event(ServerEvent::MetaUpdate {
                        title: Some(title),
                        model: None,
                    }))
                    .await;
            }
        }
        Err(e) => warn!("[worker] auto-title LLM error: {}", e),
    }
}

// ── Entry point ──

pub fn run() -> WorkerResult<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| WorkerError::Other(format!("Failed to build tokio runtime: {}", e)))?;

    runtime.block_on(run_async())
}

async fn run_async() -> WorkerResult<()> {
    let tree_id = parse_tree_id()?;

    // Read config synchronously (first line from stdin)
    let mut reader = std::io::BufReader::new(std::io::stdin());
    let mut line = String::new();
    let config = read_config(&mut reader, &mut line)?;

    // Save bytes the BufReader may have pre-fetched beyond the config line
    let stdin_saved: Vec<u8> = reader.buffer().to_vec();
    drop(reader);

    // Initialize logging
    let log_file = config.logging_to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging_level, config.logging_to_stderr);

    // Create store and run startup writes
    let store = Store::new_default(&tree_id);
    startup_writes(&store).await?;

    let session_cfg = SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };

    let cwd = resolve_repo_path(&store).await;
    let tools = tools::all_tools();
    let tool_ctx = ToolContext::new(cwd);

    // PipeOut channel → stdout writer task
    let (pipe_tx, pipe_rx) = mpsc::channel::<PipeOut>(256);
    tokio::spawn(stdout_writer(pipe_rx));

    let llm = crate::llm::WorkerLlmClient::new(pipe_tx.clone());

    let turn_ctx = Rc::new(RefCell::new(TurnCtx {
        store,
        tool_ctx,
        tools,
        pipe_tx: pipe_tx.clone(),
        llm,
        session_cfg,

    }));

    // PipeIn channel ← stdin forwarder
    let (pipe_in_tx, mut pipe_in_rx) = mpsc::channel::<PipeIn>(256);

    // Process any pre-fetched stdin bytes (complete messages only).
    // These are bytes the sync BufReader may have pre-fetched beyond the
    // config line. Parse complete PipeIn lines; incomplete leftovers are
    // discarded (the async stdin reader will re-read them from the fd).
    let mut saved_buf = stdin_saved;
    let saved_msgs = parse_pipe_messages(&mut saved_buf);

    // Spawn stdin forwarder
    tokio::spawn(stdin_forwarder(pipe_in_tx.clone()));

    // Send saved messages through the channel now that the receiver is live
    for msg in saved_msgs {
        let _ = pipe_in_tx.send(msg).await;
    }

    // Signal handler
    let (signal_tx, mut signal_rx) = mpsc::channel::<()>(1);
    tokio::spawn(signal_task(signal_tx));

    // ── Main event loop ──
    info!("[worker] entering event loop for tree {}", tree_id);

    loop {
        tokio::select! {
            Some(pipe_in) = pipe_in_rx.recv() => {
                match pipe_in {
                    PipeIn::Cmd(WsCommand::Message { params }) => {
                        info!("[worker] received Message command: {:?}", &params.text[..params.text.len().min(80)]);
                        let _ = run_turn(
                            params.text,
                            turn_ctx.clone(),
                            &mut pipe_in_rx,
                        ).await;
                    }
                    PipeIn::Cmd(WsCommand::Stop) => {
                        info!("[worker] received Stop while idle");
                        let ctx = turn_ctx.borrow();
                        ctx.tool_ctx.stop.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    PipeIn::Cmd(WsCommand::GetEntries { count }) => {
                        let ctx = turn_ctx.borrow();
                        let entries = ctx.store.read_all_entries().await.unwrap_or_default();
                        let to_emit: &[Entry] = if let Some(n) = count {
                            let len = entries.len();
                            &entries[len.saturating_sub(n)..]
                        } else {
                            &entries
                        };
                        for entry in to_emit {
                            let _ = ctx.pipe_tx
                                .send(PipeOut::Event(ServerEvent::Entry(entry.clone())))
                                .await;
                        }
                        let _ = ctx.pipe_tx
                            .send(PipeOut::Event(ServerEvent::Done {
                                status: "history".into(),
                                usage: None,
                            }))
                            .await;
                    }
                    PipeIn::Cmd(WsCommand::AutoTitle) => {
                        let ctx = turn_ctx.borrow();
                        auto_title(&ctx).await;
                    }
                    PipeIn::Llm(resp) => {
                        let ctx = turn_ctx.borrow();
                        ctx.llm.route(resp);
                    }
                    PipeIn::Config(_) => {}
                }
            }
            _ = signal_rx.recv() => {
                info!("[worker] received signal, shutting down");
                break;
            }
            else => break,
        }
    }

    Ok(())
}

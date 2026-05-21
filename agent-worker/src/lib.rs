use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{error, info, warn};
use serde::Deserialize;

use agent_core::config::SessionConfig;
use agent_core::context_files;
use agent_core::hooks;
use agent_core::rpc::{LlmRequest, LlmResponse, PipeIn, PipeOut, WsCommand};
use agent_core::store::Store;
use agent_core::tools::{self, Tool};
use agent_core::types::*;

enum AgentState {
    Idle,
    Streaming {
        messages: Vec<Message>,
        leaf_id: Option<String>,
        response_text: String,
        in_thinking: bool,
        tool_calls_buf: Vec<ToolCallBuilder>,
        finish_reason: Option<String>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
    },
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }));

    let tree_id = parse_tree_id()?;

    let mut reader = BufReader::new(std::io::stdin());
    let mut line = String::new();
    let config = read_config(&mut reader, &mut line)?;

    let log_file = config.logging_to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging_level);
    agent_core::hooks::run_startup_hooks().ok();

    let store = Store::default();
    let session_cfg = SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };
    let cwd = resolve_repo_path(&store, &tree_id);
    let tools = tools::all_tools(&cwd);
    let stop = Arc::new(AtomicBool::new(false));

    let mut out = BufWriter::new(std::io::stdout());

    let mut state = AgentState::Idle;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let msg: PipeIn = match serde_json::from_str(line.trim_end()) {
            Ok(m) => m,
            Err(e) => {
                warn!("Ignoring unparseable PipeIn: {} — line: {}", e, line.trim_end());
                continue;
            }
        };
        match &msg {
            PipeIn::Cmd(WsCommand::Message { params }) => info!("-> Cmd::Message: {}", params.text),
            PipeIn::Cmd(WsCommand::Stop) => info!("-> Cmd::Stop"),
            PipeIn::Llm(LlmResponse::Chunk { data, .. }) => log::debug!("-> Llm::Chunk: {}", data.chars().take(100).collect::<String>()),
            PipeIn::Llm(LlmResponse::Done { .. }) => info!("-> Llm::Done"),
            PipeIn::Llm(LlmResponse::Error { message, .. }) => info!("-> Llm::Error: {}", message),
            PipeIn::Config(_) => info!("-> Config"),
        }
        match msg {
            PipeIn::Cmd(WsCommand::Message { params }) => {
                if matches!(state, AgentState::Idle) {
                    state = begin_turn(
                        params.text, &tree_id, &store, &session_cfg,
                        &tools, &cwd, &stop, &mut out,
                    );
                }
            }
            PipeIn::Cmd(WsCommand::Stop) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    state = cancel_turn(state, &tree_id, &store, &stop, &mut out);
                } else {
                    // Stop with no active turn: reset the flag so it doesn't
                    // poison the next turn, but emit nothing.
                    stop.store(false, Ordering::Relaxed);
                }
            }
            PipeIn::Llm(LlmResponse::Chunk { data, .. }) => {
                if let AgentState::Streaming { .. } = &mut state {
                    process_chunk(&data, &mut state, &mut out);
                }
            }
            PipeIn::Llm(LlmResponse::Done { .. }) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    state = finish_response(
                        state, &tree_id, &store, &session_cfg,
                        &tools, &stop, &mut out,
                    );
                }
            }
            PipeIn::Llm(LlmResponse::Error { message, .. }) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    emit_event(&mut out, ServerEvent::Error { message, fatal: true });
                    state = AgentState::Idle;
                }
            }
            PipeIn::Config(_) => {}
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ConfigEnvelope {
    ch: String,
    msg: serde_json::Value,
}

fn read_config(reader: &mut BufReader<std::io::Stdin>, buf: &mut String) -> Result<agent_core::rpc::WorkerConfig, String> {
    let n = reader.read_line(buf).map_err(|e| format!("read stdin: {}", e))?;
    if n == 0 {
        return Err("stdin closed before Config".into());
    }
    let env: ConfigEnvelope = serde_json::from_str(buf.trim_end())
        .map_err(|e| format!("parse config envelope: {}", e))?;
    if env.ch != "config" {
        return Err(format!("expected Config, got ch={}", env.ch));
    }
    let cfg: agent_core::rpc::WorkerConfig = serde_json::from_value(env.msg)
        .map_err(|e| format!("parse WorkerConfig: {}", e))?;
    Ok(cfg)
}

fn emit_event(out: &mut BufWriter<std::io::Stdout>, event: ServerEvent) {
    let ev_debug = match &event {
        ServerEvent::TextChunk { content } => format!("TextChunk(len={})", content.len()),
        ServerEvent::ThinkingChunk { content } => format!("ThinkingChunk(len={})", content.len()),
        ServerEvent::ToolStart { tool, .. } => format!("ToolStart({})", tool),
        ServerEvent::ToolResult { tool, exit, .. } => format!("ToolResult({}, exit={})", tool, exit),
        ServerEvent::Entry(e) => format!("Entry({})", e.id()),
        ServerEvent::CapWarning { level, pct } => format!("CapWarning({},{}%)", level, pct),
        ServerEvent::Error { message, fatal } => format!("Error(fatal={}, {})", fatal, message),
        ServerEvent::Done { status } => format!("Done({})", status),
        ServerEvent::FileChanged { path, kind } => format!("FileChanged({},{})", kind, path),
        ServerEvent::MetaUpdate { .. } => "MetaUpdate".into(),
    };
    log::info!("emit_event: {}", ev_debug);
    let msg = PipeOut::Event(event);
    if let Ok(json) = serde_json::to_string(&msg) {
        writeln!(out, "{}", json).ok();
        out.flush().ok();
    }
}

fn send_llm_request(
    out: &mut BufWriter<std::io::Stdout>,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) {
    let n_msg = messages.len();
    let n_tools = tools.len();
    log::info!("send_llm_request: {} messages, {} tools", n_msg, n_tools);
    let req = PipeOut::Llm(LlmRequest { id: 0, messages, tools });
    if let Ok(json) = serde_json::to_string(&req) {
        log::debug!("send_llm_request pipe out: {}", json.chars().take(200).collect::<String>());
        writeln!(out, "{}", json).ok();
        out.flush().ok();
    }
}

fn begin_turn(
    text: String,
    tree_id: &str,
    store: &Store,
    session_cfg: &SessionConfig,
    tools: &[Box<dyn Tool>],
    cwd: &std::path::Path,
    _stop: &Arc<AtomicBool>,
    out: &mut BufWriter<std::io::Stdout>,
) -> AgentState {
    info!("begin_turn: tree={}, text={}", tree_id, text);
    let entries = match store.read_all_entries(tree_id) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read entries for tree {}: {}", tree_id, e);
            emit_event(out, ServerEvent::Error {
                message: format!("Failed to read entries: {}", e),
                fatal: true,
            });
            return AgentState::Idle;
        }
    };

    let tree_meta = match store.get_tree(tree_id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            error!("Tree {} not found", tree_id);
            return AgentState::Idle;
        }
        Err(e) => {
            error!("Failed to get tree {}: {}", tree_id, e);
            return AgentState::Idle;
        }
    };

    let leaf_id = tree_meta.leaf_id.clone();
    let leaf_ref = leaf_id.as_deref().unwrap_or("root");
    let mut messages = agent_core::agent::build_context(&entries, leaf_ref);

    let user_msg_id = agent_core::util::generate_entry_id();
    let mut leaf_id = leaf_id.clone();

    let user_msg = Message {
        role: MessageRole::User,
        content: MessageContent::Text(text),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
    };

    write_message_entry(store, tree_id, out, &user_msg_id, leaf_id.as_deref(), &user_msg);
    leaf_id = Some(user_msg_id.clone());

    messages.push(user_msg);

    let ctx_files = context_files::load_context_files(cwd, store.base_dir());
    let context_section = context_files::format_context_section(&ctx_files);
    messages.insert(0, Message {
        role: MessageRole::System,
        content: MessageContent::Text(build_system_prompt(&context_section)),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
    });

    if let Err(e) = hooks::run_before_llm_hooks(&mut messages) {
        warn!("Before-LLM hook blocked: {}", e);
        emit_event(out, ServerEvent::Error {
            message: format!("Hook blocked message: {}", e),
            fatal: false,
        });
        return AgentState::Idle;
    }

    let estimated = agent_core::agent::estimate_context_tokens(&messages);
    let context_window: usize = 128_000;
    let soft_cap = (session_cfg.soft_cap_pct as usize) * context_window / 100;
    let hard_cap = (session_cfg.hard_cap_pct as usize) * context_window / 100;

    info!("Context: est. {} tokens (soft={}, hard={})", estimated, soft_cap, hard_cap);

    if estimated >= soft_cap {
        let pct = (estimated * 100 / context_window) as u8;
        emit_event(out, ServerEvent::CapWarning {
            level: if estimated >= hard_cap { "hard".into() } else { "soft".into() },
            pct,
        });
        if estimated >= hard_cap {
            warn!("Hard cap reached for tree {} (est. {} tokens)", tree_id, estimated);
            emit_event(out, ServerEvent::Error {
                message: "Hard context cap reached. Ending session.".into(),
                fatal: false,
            });
            write_session_end(store, tree_id, out, SessionStatus::Continuing, None);
            return AgentState::Idle;
        }
    }

    let definitions: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();
    send_llm_request(out, messages.clone(), definitions);

    AgentState::Streaming {
        messages,
        leaf_id,
        response_text: String::new(),
        in_thinking: false,
        tool_calls_buf: vec![],
        finish_reason: None,
        tool_call_round: 0,
        tool_calls_this_turn: 0,
        consecutive_failures: 0,
    }
}

fn process_chunk(
    data: &str,
    state: &mut AgentState,
    out: &mut BufWriter<std::io::Stdout>,
) {
    let AgentState::Streaming { .. } = state else { return };

    let trimmed = data.trim();
    log::debug!("process_chunk raw data: {}", trimmed.chars().take(160).collect::<String>());
    if trimmed.is_empty() || trimmed == ":" || trimmed == "data: [DONE]" {
        return;
    }

    let raw = trimmed.strip_prefix("data: ").unwrap_or(trimmed);
    if raw.is_empty() {
        return;
    }

    let chunk: ChatChunk = match serde_json::from_str(raw) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse SSE chunk: {} — raw: {}", e, raw.chars().take(120).collect::<String>());
            return;
        }
    };

    let choice = match chunk.choices.first() {
        Some(c) => c,
        None => return,
    };

    if let AgentState::Streaming {
        ref mut response_text,
        ref mut in_thinking,
        ref mut tool_calls_buf,
        ref mut finish_reason,
        ..
    } = state
    {
        if let Some(rc) = &choice.delta.reasoning {
            if !rc.is_empty() {
                emit_event(out, ServerEvent::ThinkingChunk { content: rc.clone() });
            }
        }

        if let Some(delta) = &choice.delta.content {
            if !delta.is_empty() {
                for segment in split_thinking_chunks(delta, in_thinking) {
                    match segment {
                        ThinkingSegment::Thinking(t) => {
                            emit_event(out, ServerEvent::ThinkingChunk { content: t });
                        }
                        ThinkingSegment::Text(t) if !t.is_empty() => {
                            response_text.push_str(&t);
                            emit_event(out, ServerEvent::TextChunk { content: t });
                        }
                        _ => {}
                    }
                }
            }
        }

        for tc_delta in &choice.delta.tool_calls {
            let idx = tc_delta.index.unwrap_or(0) as usize;
            while tool_calls_buf.len() <= idx {
                tool_calls_buf.push(ToolCallBuilder::default());
            }
            let builder = &mut tool_calls_buf[idx];
            if let Some(id) = &tc_delta.id {
                builder.id = id.clone();
            }
            if let Some(name) = &tc_delta.function.name {
                builder.name = name.clone();
            }
            if let Some(args) = &tc_delta.function.arguments {
                builder.arguments.push_str(args);
            }
        }

        if let Some(reason) = &choice.finish_reason {
            if !reason.is_empty() {
                *finish_reason = Some(reason.clone());
            }
        }
    }
}

fn finish_response(
    state: AgentState,
    tree_id: &str,
    store: &Store,
    session_cfg: &SessionConfig,
    tools: &[Box<dyn Tool>],
    stop: &Arc<AtomicBool>,
    out: &mut BufWriter<std::io::Stdout>,
) -> AgentState {
    let AgentState::Streaming {
        mut messages,
        mut leaf_id,
        response_text,
        in_thinking: _,
        tool_calls_buf,
        finish_reason,
        mut tool_call_round,
        mut tool_calls_this_turn,
        mut consecutive_failures,
    } = state else {
        return AgentState::Idle;
    };

    let reason = finish_reason.as_deref().unwrap_or("stop");

    match reason {
        "tool_calls" => {
            let completed_calls: Vec<ToolCall> = tool_calls_buf.iter().map(|b| {
                ToolCall {
                    id: b.id.clone(),
                    name: b.name.clone(),
                    arguments: serde_json::from_str(&b.arguments)
                        .unwrap_or(serde_json::Value::Null),
                }
            }).collect();

            let msg_id = agent_core::util::generate_entry_id();
            let assistant_msg = Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text(response_text.clone()),
                tool_calls: Some(completed_calls.clone()),
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            };

            write_message_entry(store, tree_id, out, &msg_id, leaf_id.as_deref(), &assistant_msg);
            leaf_id = Some(msg_id);
            messages.push(assistant_msg);

            tool_call_round += 1;
            let max_per_turn = session_cfg.max_tool_calls_per_turn;

            for call in &completed_calls {
                tool_calls_this_turn += 1;
                if tool_calls_this_turn > max_per_turn {
                    warn!("Max tool calls per turn reached for tree {}", tree_id);
                    return AgentState::Idle;
                }

                if let Err(e) = hooks::run_tool_call_hooks(&call.name, &call.arguments) {
                    emit_event(out, ServerEvent::Error {
                        message: format!("Hook blocked tool call: {}", e),
                        fatal: false,
                    });
                    consecutive_failures += 1;
                    continue;
                }

                emit_event(out, ServerEvent::ToolStart {
                    tool: call.name.clone(),
                    input: call.arguments.clone(),
                });

                let result = execute_tool(tools, &call.name, &call.arguments, stop);

                if result.exit_code.unwrap_or(0) != 0 {
                    consecutive_failures += 1;
                    if consecutive_failures >= 3 {
                        warn!("3 consecutive tool failures for tree {}", tree_id);
                        emit_event(out, ServerEvent::Error {
                            message: "3 consecutive tool failures, aborting turn".into(),
                            fatal: false,
                        });
                        return AgentState::Idle;
                    }
                } else {
                    consecutive_failures = 0;
                }

                emit_event(out, ServerEvent::ToolResult {
                    tool: call.name.clone(),
                    exit: result.exit_code.unwrap_or(0),
                    output: preview_tool_output(&result),
                });

                if call.name == "bash" {
                    let exit_code = result.exit_code.unwrap_or(0);
                    let bash_entry = Entry::BashExec {
                        id: agent_core::util::generate_entry_id(),
                        parent_id: leaf_id.clone(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        command: call.arguments.get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        output: result.content.clone(),
                        exit_code,
                        truncated: result.truncated,
                        duration_ms: None,
                    };
                    if let Err(e) = store.append_entry(tree_id, &bash_entry) {
                        error!("Failed to append bash_exec: {}", e);
                    }
                    emit_event(out, ServerEvent::Entry(bash_entry));
                }

                let tool_result_msg = Message {
                    role: MessageRole::Tool,
                    content: MessageContent::Text(format_tool_output(&result)),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    tool_name: Some(call.name.clone()),
                    usage: None,
                    stop_reason: None,
                    is_error: if result.exit_code.unwrap_or(0) != 0 { Some(true) } else { None },
                };

                messages.push(tool_result_msg);
            }

            if consecutive_failures >= 3 || tool_call_round >= max_per_turn {
                if tool_call_round >= max_per_turn {
                    warn!("Max tool call rounds ({}) reached for tree {}", max_per_turn, tree_id);
                    emit_event(out, ServerEvent::Error {
                        message: format!("Max tool call rounds ({}) reached", max_per_turn),
                        fatal: false,
                    });
                }
                return AgentState::Idle;
            }

            let definitions: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();
            send_llm_request(out, messages.clone(), definitions);

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
            }
        }
        "stop" | "length" => {
            emit_event(out, ServerEvent::Done { status: reason.to_string() });

            if !response_text.is_empty() {
                let msg_id = agent_core::util::generate_entry_id();
                let assistant_msg = Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(response_text.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                    tool_name: None,
                    usage: None,
                    stop_reason: None,
                    is_error: None,
                };
                write_message_entry(store, tree_id, out, &msg_id, leaf_id.as_deref(), &assistant_msg);
                leaf_id = Some(msg_id);
            }

            if let Ok(Some(mut meta)) = store.get_tree(tree_id) {
                meta.leaf_id = leaf_id.clone();
                meta.updated_at = chrono::Utc::now().timestamp();
                let _ = store.save_tree_meta(&meta);
            }

            AgentState::Idle
        }
        _ => {
            warn!("Unknown finish reason '{}' for tree {}", reason, tree_id);
            emit_event(out, ServerEvent::Done { status: reason.to_string() });
            AgentState::Idle
        }
    }
}

fn cancel_turn(
    state: AgentState,
    tree_id: &str,
    store: &Store,
    stop: &Arc<AtomicBool>,
    out: &mut BufWriter<std::io::Stdout>,
) -> AgentState {
    if let AgentState::Streaming { response_text, leaf_id, .. } = &state {
        if !response_text.is_empty() {
            let msg_id = agent_core::util::generate_entry_id();
            let assistant_msg = Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text(response_text.clone()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            };
            write_message_entry(store, tree_id, out, &msg_id, leaf_id.as_deref(), &assistant_msg);
        }
    }

    emit_event(out, ServerEvent::Done { status: "cancelled".into() });
    stop.store(false, Ordering::Relaxed);
    AgentState::Idle
}

fn build_system_prompt(context_section: &str) -> String {
    format!(
        "You are a coding agent. You work in a repository and have access to tools.\n\
         \n\
         ## Available Tools\n\
         \n\
         You can use the following tools to interact with the repository:\n\
         - `read`: Read file contents (supports offset/limit for large files)\n\
         - `write`: Write contents to a file (creates parent directories)\n\
         - `edit`: Edit a file with text replacement (exact match first, fuzzy fallback)\n\
         - `bash`: Execute shell commands (use for builds, tests, git operations)\n\
         - `ls`: List directory contents\n\
         - `grep`: Search file contents with regex\n\
         - `find`: Find files by pattern\n\
         - `git`: Git operations (status, diff, log, add, commit, push, pull)\n\
         - `search_messages`: Search past session messages\n\
         - `search_files`: Search for files across all sessions\n\
         \n\
         ## Guidelines\n\
         \n\
         1. Read files, understand what you're working with\n\
         2. Use the right tool for the job\n\
         3. Run commands to verify your work\n\
         4. Write clear, concise code\n\
         5. Use `edit` for targeted changes instead of rewriting entire files\n\
         6. When output is truncated, use more specific queries\n\
         \n\
         {}",
        context_section
    )
}

fn execute_tool(tools: &[Box<dyn Tool>], name: &str, args: &serde_json::Value, stop: &Arc<AtomicBool>) -> ToolOutput {
    let tool = match tools.iter().find(|t| t.definition().name == name) {
        Some(t) => t,
        None => {
            return ToolOutput {
                content: format!("Error: Unknown tool '{}'", name),
                truncated: false,
                original_size: 0,
                exit_code: Some(1),
            };
        }
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.execute(args, stop))) {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => ToolOutput {
            content: format!("Error: {}", e),
            truncated: false,
            original_size: 0,
            exit_code: Some(1),
        },
        Err(_) => ToolOutput {
            content: format!("Error: Tool '{}' panicked", name),
            truncated: false,
            original_size: 0,
            exit_code: Some(1),
        },
    }
}

fn format_tool_output(output: &ToolOutput) -> String {
    if output.truncated {
        let preview: String = output.content.chars().take(2000).collect();
        format!(
            "{}\n\n(Output truncated: was {} bytes)",
            preview, output.original_size
        )
    } else {
        output.content.clone()
    }
}

fn preview_tool_output(output: &ToolOutput) -> String {
    if output.truncated {
        let preview: String = output.content.chars().take(2000).collect();
        format!("{}... (truncated, was {} bytes)", preview, output.original_size)
    } else {
        output.content.clone()
    }
}

fn resolve_repo_path(store: &Store, tree_id: &str) -> std::path::PathBuf {
    match store.get_tree(tree_id) {
        Ok(Some(meta)) => {
            if let Some(repo_path) = &meta.repo_path {
                if repo_path.exists() {
                    return repo_path.clone();
                }
                warn!(
                    "Repo path {:?} for tree {} does not exist, using cwd",
                    repo_path, tree_id
                );
            }
        }
        Ok(None) => warn!("Tree {} not found, using cwd", tree_id),
        Err(e) => warn!("Failed to get tree {}: {}, using cwd", tree_id, e),
    }
    std::env::current_dir().unwrap_or_else(|_| store.base_dir().clone())
}

fn write_session_end(
    store: &Store,
    tree_id: &str,
    out: &mut BufWriter<std::io::Stdout>,
    status: SessionStatus,
    continuation_brief: Option<String>,
) {
    let entry = Entry::SessionEnd {
        id: agent_core::util::generate_entry_id(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: None,
        status,
        continuation_brief,
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        error!("Failed to write session_end for tree {}: {}", tree_id, e);
    }
    emit_event(out, ServerEvent::Entry(entry));
}

fn write_message_entry(
    store: &Store,
    tree_id: &str,
    out: &mut BufWriter<std::io::Stdout>,
    entry_id: &str,
    parent_id: Option<&str>,
    message: &Message,
) {
    let entry = Entry::Message {
        id: entry_id.to_string(),
        parent_id: parent_id.map(|s| s.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        message: message.clone(),
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        error!("Failed to append message entry for tree {}: {}", tree_id, e);
    }
    emit_event(out, ServerEvent::Entry(entry));
}

enum ThinkingSegment {
    Thinking(String),
    Text(String),
}

fn split_thinking_chunks(text: &str, in_thinking: &mut bool) -> Vec<ThinkingSegment> {
    let mut result = Vec::new();
    let mut rest = text;
    loop {
        if *in_thinking {
            match rest.find("</think>") {
                Some(pos) => {
                    if pos > 0 {
                        result.push(ThinkingSegment::Thinking(rest[..pos].to_string()));
                    }
                    *in_thinking = false;
                    rest = &rest[pos + "</think>".len()..];
                }
                None => {
                    if !rest.is_empty() {
                        result.push(ThinkingSegment::Thinking(rest.to_string()));
                    }
                    break;
                }
            }
        } else {
            match rest.find("<think>") {
                Some(pos) => {
                    if pos > 0 {
                        result.push(ThinkingSegment::Text(rest[..pos].to_string()));
                    }
                    *in_thinking = true;
                    rest = &rest[pos + "<think>".len()..];
                }
                None => {
                    if !rest.is_empty() {
                        result.push(ThinkingSegment::Text(rest.to_string()));
                    }
                    break;
                }
            }
        }
    }
    result
}

fn parse_tree_id() -> Result<String, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut tree_id = None;
    while let Some(arg) = args.next() {
        if arg.as_str() == "--tree-id" {
            tree_id = Some(args.next().ok_or("missing --tree-id value")?);
        }
    }
    tree_id.ok_or_else(|| "--tree-id is required".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_no_tags() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("hello world", &mut in_thinking);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], ThinkingSegment::Text(t) if t == "hello world"));
        assert!(!in_thinking);
    }

    #[test]
    fn test_split_full_block() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("<think>reason</think>answer", &mut in_thinking);
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], ThinkingSegment::Thinking(t) if t == "reason"));
        assert!(matches!(&result[1], ThinkingSegment::Text(t) if t == "answer"));
        assert!(!in_thinking);
    }

    #[test]
    fn test_split_open_only() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("<think>partial", &mut in_thinking);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], ThinkingSegment::Thinking(t) if t == "partial"));
        assert!(in_thinking);
    }

    #[test]
    fn test_split_close_only() {
        let mut in_thinking = true;
        let result = split_thinking_chunks("end</think>rest", &mut in_thinking);
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], ThinkingSegment::Thinking(t) if t == "end"));
        assert!(matches!(&result[1], ThinkingSegment::Text(t) if t == "rest"));
        assert!(!in_thinking);
    }

    #[test]
    fn test_split_empty_think_block() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("<think></think>after", &mut in_thinking);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], ThinkingSegment::Text(t) if t == "after"));
        assert!(!in_thinking);
    }
}
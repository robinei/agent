use std::io::BufWriter;
use std::sync::atomic::Ordering;

use agent_core::config::SessionConfig;
use agent_core::store::Store;
use agent_core::types::*;
use log::{error, info, warn};

use crate::lsp_client::{
    default_server, detect_language, format_diagnostics, binary_exists,
    LspClient, LspFileResult, LspWaitState, PendingLspTool,
};
use crate::thinking::{split_thinking_chunks, ThinkingSegment};
use agent_core::types::NotificationLevel;
use crate::util::{emit_notification, emit_event, send_llm_request, write_message_entry, write_session_end};
use crate::AgentState;
use crate::tools::ToolOutput;

fn make_assistant_message(text: String, tool_calls: Option<Vec<ToolCall>>) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: MessageContent::Text(text),
        tool_calls,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
    }
}

fn make_user_message(text: String) -> Message {
    Message {
        role: MessageRole::User,
        content: MessageContent::Text(text),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
    }
}

pub fn make_tool_result_message(tool_call_id: &str, tool_name: &str, result: &Result<String, String>) -> Message {
    let (content, is_error) = match result {
        Ok(c) => (c.clone(), None),
        Err(e) => (format!("Error: {}", e), Some(true)),
    };
    Message {
        role: MessageRole::Tool,
        content: MessageContent::Text(content),
        tool_calls: None,
        tool_call_id: Some(tool_call_id.to_string()),
        tool_name: Some(tool_name.to_string()),
        usage: None,
        stop_reason: None,
        is_error,
    }
}

fn collect_tool_definitions(tools: &[Box<dyn crate::tools::Tool>]) -> Vec<ToolDefinition> {
    tools.iter().map(|t| t.definition()).collect()
}

fn build_system_prompt(repo_path: &std::path::Path, context_section: &str) -> String {
    let is_git = repo_path.join(".git").exists();
    format!(
        "You are a coding agent working in a repository. Always respond in English.\n\
         Repo path: {}\n\
         Version control: {}\n\
         When listing files, prefer `rg --files` over `find` — it respects .gitignore.\n\
         \n\
         {}",
        repo_path.display(),
        if is_git { "git" } else { "none — this is not a git repository, do not run git commands" },
        context_section
    )
}

fn check_context_cap(
    estimated: usize,
    session_cfg: &SessionConfig,
    tree_id: &str,
    store: &Store,
    out: &mut BufWriter<std::io::Stdout>,
) -> Result<(), ()> {
    let context_window: usize = 128_000;
    let soft_cap = (session_cfg.soft_cap_pct as usize) * context_window / 100;
    let hard_cap = (session_cfg.hard_cap_pct as usize) * context_window / 100;

    info!(
        "Context: est. {} tokens (soft={}, hard={})",
        estimated, soft_cap, hard_cap
    );

    if estimated >= soft_cap {
        let pct = (estimated * 100 / context_window) as u8;
        emit_event(
            out,
            ServerEvent::CapWarning {
                level: if estimated >= hard_cap {
                    "hard".into()
                } else {
                    "soft".into()
                },
                pct,
            },
        );
        if estimated >= hard_cap {
            warn!("Hard cap reached for tree {} (est. {} tokens)", tree_id, estimated);
            emit_notification(out, NotificationLevel::Error, "Hard context cap reached. Ending session.".into());
            write_session_end(store, tree_id, out, SessionStatus::Continuing, None);
            return Err(());
        }
    }
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

pub fn begin_turn(
    text: String,
    tree_id: &str,
    store: &Store,
    session_cfg: &SessionConfig,
    tools: &[Box<dyn crate::tools::Tool>],
    ctx: &mut crate::tools::ToolContext,
    out: &mut BufWriter<std::io::Stdout>,
) -> AgentState {
    info!("begin_turn: tree={}, text={}", tree_id, text);
    let entries = match store.read_all_entries(tree_id) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read entries for tree {}: {}", tree_id, e);
            emit_notification(out, NotificationLevel::Fatal, format!("Failed to read entries: {}", e));
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
    let mut messages = crate::agent::build_context(&entries, leaf_ref);

    let user_msg_id = agent_core::util::generate_entry_id();
    let mut leaf_id = leaf_id.clone();

    let user_msg = make_user_message(text);

    write_message_entry(store, tree_id, out, &user_msg_id, leaf_id.as_deref(), &user_msg);
    leaf_id = Some(user_msg_id.clone());

    messages.push(user_msg);

    let ctx_files = crate::context_files::load_context_files(&ctx.cwd, store.base_dir());
    let context_section = crate::context_files::format_context_section(&ctx_files);
    messages.insert(
        0,
        Message {
            role: MessageRole::System,
            content: MessageContent::Text(build_system_prompt(&ctx.cwd, &context_section)),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
        },
    );

    let estimated = crate::agent::estimate_context_tokens(&messages);
    if check_context_cap(estimated, session_cfg, tree_id, store, out).is_err() {
        return AgentState::Idle;
    }

    let definitions = collect_tool_definitions(tools);
    send_llm_request(out, messages.clone(), definitions);

    AgentState::new_streaming(messages, leaf_id, 0, 0, 0)
}

fn execute_tool(
    tools: &[Box<dyn crate::tools::Tool>],
    name: &str,
    args: &serde_json::Value,
    ctx: &mut crate::tools::ToolContext,
) -> Result<String, String> {
    let tool = match tools.iter().find(|t| t.definition().name == name) {
        Some(t) => t,
        None => return Err(format!("Unknown tool '{}'", name)),
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.execute(args, ctx))) {
        Ok(ToolOutput::Done(result)) => result,
        Ok(ToolOutput::PendingLsp { .. }) => {
            Err("unexpected PendingLsp from tool that doesn't implement LSP".into())
        }
        Err(_) => Err(format!("Tool '{}' panicked", name)),
    }
}

fn notify_lsp_saves(
    ctx: &mut crate::tools::ToolContext,
    lsp_cfg: &LspConfig,
    dirty: &[std::path::PathBuf],
    pending_tools: &[PendingLspTool],
    out: &mut BufWriter<std::io::Stdout>,
) -> (u64, u64) {
    let mut max_timeout = 5000u64;
    let mut max_silence = 500u64;
    let mut resolved_lang_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for lang_id in pending_tools.iter().map(|p| p.lang_id.clone()) {
        resolved_lang_ids.insert(lang_id);
    }

    let mut lang_groups: std::collections::HashMap<String, Vec<std::path::PathBuf>> =
        std::collections::HashMap::new();
    for path in dirty {
        if let Some(lang_id) = detect_language(path) {
            lang_groups.entry(lang_id.to_string()).or_default().push(path.clone());
        }
    }

    for (lang_id, paths) in lang_groups {
        resolved_lang_ids.insert(lang_id.clone());

        if ctx.lsp_clients.contains_key(&lang_id) {
            if let Some(client) = ctx.lsp_clients.get_mut(&lang_id) {
                for p in &paths {
                    client.notify_saved(p);
                }
            }
            continue;
        }

        let server_cfg = lsp_cfg.servers.iter()
            .find(|s| s.language == lang_id)
            .cloned()
            .or_else(|| default_server(&lang_id));

        let Some(ref cfg) = server_cfg else {
            warn!("No LSP server config for language '{}'", lang_id);
            continue;
        };

        if !binary_exists(&cfg.command) {
            emit_notification(out, NotificationLevel::Warning,
                format!("LSP: '{}' not found — skipping diagnostics for {}", cfg.command, lang_id));
            continue;
        }

        let root_uri = format!("file://{}", ctx.cwd.display());
        match LspClient::spawn(&lang_id, &cfg.command, &cfg.args, &root_uri, cfg.timeout_ms) {
            Ok(client) => {
                emit_notification(out, NotificationLevel::Info,
                    format!("LSP: started {} for {}", cfg.command, lang_id));
                max_timeout = max_timeout.max(cfg.timeout_ms);
                max_silence = max_silence.max(cfg.silence_ms);
                ctx.lsp_clients.insert(lang_id.clone(), client);
                if let Some(client) = ctx.lsp_clients.get_mut(&lang_id) {
                    for p in &paths {
                        client.notify_saved(p);
                    }
                }
            }
            Err(e) => emit_notification(out, NotificationLevel::Warning,
                format!("LSP: failed to start {} for {}: {}", cfg.command, lang_id, e)),
        }
    }

    for lang_id in &resolved_lang_ids {
        if ctx.lsp_clients.contains_key(lang_id) {
            let cfg = lsp_cfg.servers.iter()
                .find(|s| s.language == *lang_id)
                .cloned()
                .or_else(|| default_server(lang_id));
            if let Some(c) = &cfg {
                max_timeout = max_timeout.max(c.timeout_ms);
                max_silence = max_silence.max(c.silence_ms);
            }
        }
    }

    (max_timeout, max_silence)
}

pub fn process_chunk(
    data: &str,
    state: &mut AgentState,
    out: &mut BufWriter<std::io::Stdout>,
) {
    let AgentState::Streaming { .. } = state else { return };

    let trimmed = data.trim();
    log::debug!(
        "process_chunk data: {}",
        trimmed.chars().take(160).collect::<String>()
    );
    if trimmed.is_empty() {
        return;
    }

    let chunk: ChatChunk = match serde_json::from_str(trimmed) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "Failed to parse ChatChunk: {} — raw: {}",
                e,
                trimmed.chars().take(120).collect::<String>()
            );
            return;
        }
    };

    if let AgentState::Streaming {
        ref mut response_text,
        ref mut in_thinking,
        ref mut tool_calls_buf,
        ref mut finish_reason,
        ..
    } = state
    {
        if let Some(rc) = &chunk.delta_reasoning {
            if !rc.is_empty() {
                emit_event(out, ServerEvent::ThinkingChunk { content: rc.clone() });
            }
        }

        if let Some(delta) = &chunk.delta_text {
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

        for tc_delta in &chunk.tool_call_delta {
            let idx = tc_delta.index.unwrap_or(0) as usize;
            while tool_calls_buf.len() <= idx {
                tool_calls_buf.push(agent_core::types::ToolCallBuilder::default());
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

        if let Some(reason) = chunk.finish_reason {
            *finish_reason = Some(reason);
        }
    }
}

pub fn finish_response(
    state: AgentState,
    tree_id: &str,
    store: &Store,
    session_cfg: &SessionConfig,
    tools: &[Box<dyn crate::tools::Tool>],
    ctx: &mut crate::tools::ToolContext,
    out: &mut BufWriter<std::io::Stdout>,
    lsp_cfg: &LspConfig,
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
        lsp_wait: _,
    } = state
    else {
        return AgentState::Idle;
    };

    match finish_reason.unwrap_or(StopReason::Stop) {
        StopReason::ToolCalls => {
            let completed_calls: Vec<ToolCall> = tool_calls_buf
                .iter()
                .map(|b| ToolCall {
                    id: b.id.clone(),
                    name: b.name.clone(),
                    arguments: serde_json::from_str(&b.arguments)
                        .unwrap_or(serde_json::Value::Null),
                })
                .collect();

            let msg_id = agent_core::util::generate_entry_id();
            let assistant_msg =
                make_assistant_message(response_text.clone(), Some(completed_calls.clone()));

            write_message_entry(store, tree_id, out, &msg_id, leaf_id.as_deref(), &assistant_msg);
            leaf_id = Some(msg_id);
            messages.push(assistant_msg);

            tool_call_round += 1;
            let max_per_turn = session_cfg.max_tool_calls_per_turn;
            let mut pending_lsp_tools: Vec<PendingLspTool> = Vec::new();

            for call in &completed_calls {
                tool_calls_this_turn += 1;
                if tool_calls_this_turn > max_per_turn {
                    warn!("Max tool calls per turn reached for tree {}", tree_id);
                    emit_notification(out, NotificationLevel::Error, format!("Max tool calls per turn ({}) reached", max_per_turn));
                    emit_event(out, ServerEvent::Done { status: "error".into() });
                    return AgentState::Idle;
                }

                emit_event(
                    out,
                    ServerEvent::ToolStart {
                        tool: call.name.clone(),
                        input: call.arguments.clone(),
                    },
                );

                let tool = tools.iter().find(|t| t.definition().name == call.name);
                let tool = match tool {
                    Some(t) => t,
                    None => {
                        emit_event(out, ServerEvent::ToolResult {
                            tool: call.name.clone(), exit: 1,
                            output: format!("Unknown tool '{}'", call.name),
                        });
                        let err_msg = make_tool_result_message(&call.id, &call.name, &Err(format!("Unknown tool '{}'", call.name)));
                        let result_msg_id = agent_core::util::generate_entry_id();
                        write_message_entry(store, tree_id, out, &result_msg_id, leaf_id.as_deref(), &err_msg);
                        leaf_id = Some(result_msg_id);
                        messages.push(err_msg);
                        consecutive_failures += 1;
                        if consecutive_failures >= 3 {
                            warn!("3 consecutive tool failures for tree {}", tree_id);
                            emit_notification(out, NotificationLevel::Error, "3 consecutive tool failures, aborting turn".into());
                            emit_event(out, ServerEvent::Done { status: "error".into() });
                            return AgentState::Idle;
                        }
                        continue;
                    }
                };

                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.execute(&call.arguments, ctx))) {
                    Ok(output) => output,
                    Err(_) => {
                        emit_event(out, ServerEvent::ToolResult {
                            tool: call.name.clone(), exit: 1,
                            output: format!("Tool '{}' panicked", call.name),
                        });
                        let err_msg = make_tool_result_message(&call.id, &call.name, &Err(format!("Tool '{}' panicked", call.name)));
                        let result_msg_id = agent_core::util::generate_entry_id();
                        write_message_entry(store, tree_id, out, &result_msg_id, leaf_id.as_deref(), &err_msg);
                        leaf_id = Some(result_msg_id);
                        messages.push(err_msg);
                        consecutive_failures += 1;
                        if consecutive_failures >= 3 {
                            warn!("3 consecutive tool failures for tree {}", tree_id);
                            emit_notification(out, NotificationLevel::Error, "3 consecutive tool failures, aborting turn".into());
                            emit_event(out, ServerEvent::Done { status: "error".into() });
                            return AgentState::Idle;
                        }
                        continue;
                    }
                };

                match result {
                    ToolOutput::Done(res) => {
                        let is_error = res.is_err();
                        if is_error {
                            consecutive_failures += 1;
                            if consecutive_failures >= 3 {
                                warn!("3 consecutive tool failures for tree {}", tree_id);
                                emit_notification(out, NotificationLevel::Error, "3 consecutive tool failures, aborting turn".into());
                                emit_event(out, ServerEvent::Done { status: "error".into() });
                                return AgentState::Idle;
                            }
                        } else {
                            consecutive_failures = 0;
                        }

                        let content = match &res {
                            Ok(c) => c.clone(),
                            Err(e) => e.clone(),
                        };

                        emit_event(
                            out,
                            ServerEvent::ToolResult {
                                tool: call.name.clone(),
                                exit: if is_error { 1 } else { 0 },
                                output: {
                                    let preview: String = content.chars().take(2000).collect();
                                    if content.len() > 2000 {
                                        format!("{}... (truncated, was {} bytes)", preview, content.len())
                                    } else {
                                        content.clone()
                                    }
                                },
                            },
                        );

                        if call.name == "bash" {
                            let exit_code = if is_error { 1 } else { parse_exit_code(&content) };
                            let bash_entry = Entry::BashExec {
                                id: agent_core::util::generate_entry_id(),
                                parent_id: leaf_id.clone(),
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                command: call
                                    .arguments
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                output: content.clone(),
                                exit_code,
                                truncated: content.contains("Output truncated"),
                                duration_ms: None,
                            };
                            if let Err(e) = store.append_entry(tree_id, &bash_entry) {
                                error!("Failed to append bash_exec: {}", e);
                            }
                            emit_event(out, ServerEvent::Entry(bash_entry));
                        }

                        let tool_result_msg = make_tool_result_message(&call.id, &call.name, &res);
                        let result_msg_id = agent_core::util::generate_entry_id();
                        write_message_entry(store, tree_id, out, &result_msg_id, leaf_id.as_deref(), &tool_result_msg);
                        leaf_id = Some(result_msg_id);
                        messages.push(tool_result_msg);
                    }
                    ToolOutput::PendingLsp { request_id, lang_id } => {
                        pending_lsp_tools.push(PendingLspTool {
                            request_id,
                            lang_id,
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                        });
                    }
                }
            }

            if consecutive_failures >= 3 || tool_call_round >= max_per_turn {
                if tool_call_round >= max_per_turn {
                    warn!(
                        "Max tool call rounds ({}) reached for tree {}",
                        max_per_turn, tree_id
                    );
                    emit_notification(out, NotificationLevel::Error, format!("Max tool call rounds ({}) reached", max_per_turn));
                    emit_event(out, ServerEvent::Done { status: "error".into() });
                }
                return AgentState::Idle;
            }

            // Check if we need LSP wait
            let dirty = std::mem::take(&mut ctx.lsp_dirty);
            let needs_lsp_wait = lsp_cfg.enabled && (!dirty.is_empty() || !pending_lsp_tools.is_empty());
            if needs_lsp_wait {
                let (timeout_ms, silence_ms) = notify_lsp_saves(ctx, lsp_cfg, &dirty, &pending_lsp_tools, out);
                return AgentState::Streaming {
                    messages, leaf_id,
                    response_text: String::new(), in_thinking: false,
                    tool_calls_buf: vec![], finish_reason: None,
                    tool_call_round, tool_calls_this_turn, consecutive_failures,
                    lsp_wait: Some(LspWaitState {
                        deadline: std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms),
                        silence_until: std::time::Instant::now() + std::time::Duration::from_millis(silence_ms),
                        silence_ms,
                        pending_tool_requests: pending_lsp_tools,
                    }),
                };
            }

            let definitions = collect_tool_definitions(tools);
            send_llm_request(out, messages.clone(), definitions);

            AgentState::new_streaming(messages, leaf_id, tool_call_round, tool_calls_this_turn, consecutive_failures)
        }
        reason @ (StopReason::Stop | StopReason::Length) => {
            let status = if matches!(reason, StopReason::Length) { "length" } else { "stop" };
            emit_event(out, ServerEvent::Done { status: status.into() });

            if !response_text.is_empty() {
                let msg_id = agent_core::util::generate_entry_id();
                let assistant_msg = make_assistant_message(response_text.clone(), None);
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
        reason => {
            warn!("Unhandled finish reason {:?} for tree {}", reason, tree_id);
            emit_event(out, ServerEvent::Done { status: "stop".into() });
            AgentState::Idle
        }
    }
}

pub fn resolve_lsp_wait_into(
    state: AgentState,
    lsp_clients: &std::collections::HashMap<String, LspClient>,
    out: &mut BufWriter<std::io::Stdout>,
    tools: &[Box<dyn crate::tools::Tool>],
) -> AgentState {
    let AgentState::Streaming {
        mut messages, leaf_id, tool_call_round,
        tool_calls_this_turn, consecutive_failures, ..
    } = state else { return state };

    let results: Vec<LspFileResult> = lsp_clients.values()
        .flat_map(|c| c.all_diagnostics())
        .collect();
    if !results.is_empty() {
        let diag_text = format_diagnostics(&results);
        emit_event(out, ServerEvent::ToolResult {
            tool: "lsp_diagnostics".to_string(),
            exit: 0,
            output: diag_text.clone(),
        });
        // Inject as a user message — a tool result without a matching tool_use
        // call would be rejected by the API.
        messages.push(Message {
            role: MessageRole::User,
            content: MessageContent::Text(format!("[LSP diagnostics]\n{}", diag_text)),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            usage: None,
            stop_reason: None,
            is_error: None,
        });
    }
    let definitions = tools.iter().map(|t| t.definition()).collect();
    send_llm_request(out, messages.clone(), definitions);
    AgentState::new_streaming(messages, leaf_id, tool_call_round, tool_calls_this_turn, consecutive_failures)
}

pub fn resolve_lsp_wait_with_timeout(
    state: AgentState,
    ctx: &mut crate::tools::ToolContext,
    out: &mut BufWriter<std::io::Stdout>,
    tools: &[Box<dyn crate::tools::Tool>],
) -> AgentState {
    let AgentState::Streaming {
        mut messages, leaf_id, tool_call_round,
        tool_calls_this_turn, consecutive_failures, lsp_wait: Some(ref wait), ..
    } = state else { return state };

    for pending in &wait.pending_tool_requests {
        let err_msg = make_tool_result_message(
            &pending.tool_call_id, &pending.tool_name,
            &Err("LSP request timed out".into()),
        );
        messages.push(err_msg);
    }

    resolve_lsp_wait_into(
        AgentState::Streaming {
            messages, leaf_id, tool_call_round,
            tool_calls_this_turn, consecutive_failures,
            response_text: String::new(), in_thinking: false,
            tool_calls_buf: vec![], finish_reason: None,
            lsp_wait: None,
        },
        &ctx.lsp_clients, out, tools,
    )
}

pub fn cancel_turn(
    state: AgentState,
    tree_id: &str,
    store: &Store,
    ctx: &mut crate::tools::ToolContext,
    out: &mut BufWriter<std::io::Stdout>,
) -> AgentState {
    if let AgentState::Streaming {
        response_text,
        leaf_id,
        ..
    } = &state
    {
        if !response_text.is_empty() {
            let msg_id = agent_core::util::generate_entry_id();
            let assistant_msg = make_assistant_message(response_text.clone(), None);
            write_message_entry(store, tree_id, out, &msg_id, leaf_id.as_deref(), &assistant_msg);
        }
    }

    emit_event(out, ServerEvent::Done { status: "cancelled".into() });
    ctx.stop.store(false, Ordering::Relaxed);
    AgentState::Idle
}
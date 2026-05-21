use std::io::BufWriter;
use std::sync::atomic::Ordering;

use agent_core::config::SessionConfig;
use agent_core::store::Store;
use agent_core::types::*;
use log::{error, info, warn};

use crate::thinking::{split_thinking_chunks, ThinkingSegment};
use crate::util::{emit_error, emit_event, send_llm_request, write_message_entry, write_session_end};
use crate::AgentState;

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

fn make_tool_result_message(output: &ToolOutput, call: &ToolCall) -> Message {
    Message {
        role: MessageRole::Tool,
        content: MessageContent::Text(format_tool_output(output)),
        tool_calls: None,
        tool_call_id: Some(call.id.clone()),
        tool_name: Some(call.name.clone()),
        usage: None,
        stop_reason: None,
        is_error: if output.exit_code.unwrap_or(0) != 0 {
            Some(true)
        } else {
            None
        },
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
            emit_error(out, "Hard context cap reached. Ending session.".into(), false);
            write_session_end(store, tree_id, out, SessionStatus::Continuing, None);
            return Err(());
        }
    }
    Ok(())
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
            emit_error(
                out,
                format!("Failed to read entries: {}", e),
                true,
            );
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
) -> ToolOutput {
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

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.execute(args, ctx))) {
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
        format!(
            "{}... (truncated, was {} bytes)",
            preview, output.original_size
        )
    } else {
        output.content.clone()
    }
}

pub fn process_chunk(
    data: &str,
    state: &mut AgentState,
    out: &mut BufWriter<std::io::Stdout>,
) {
    let AgentState::Streaming { .. } = state else { return };

    let trimmed = data.trim();
    log::debug!(
        "process_chunk raw data: {}",
        trimmed.chars().take(160).collect::<String>()
    );
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
            warn!(
                "Failed to parse SSE chunk: {} — raw: {}",
                e,
                raw.chars().take(120).collect::<String>()
            );
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

pub fn finish_response(
    state: AgentState,
    tree_id: &str,
    store: &Store,
    session_cfg: &SessionConfig,
    tools: &[Box<dyn crate::tools::Tool>],
    ctx: &mut crate::tools::ToolContext,
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
    } = state
    else {
        return AgentState::Idle;
    };

    let reason = finish_reason.as_deref().unwrap_or("stop");

    match reason {
        "tool_calls" => {
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

            for call in &completed_calls {
                tool_calls_this_turn += 1;
                if tool_calls_this_turn > max_per_turn {
                    warn!("Max tool calls per turn reached for tree {}", tree_id);
                    emit_error(out, format!("Max tool calls per turn ({}) reached", max_per_turn), false);
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

                let result = execute_tool(tools, &call.name, &call.arguments, ctx);

                if result.exit_code.unwrap_or(0) != 0 {
                    consecutive_failures += 1;
                    if consecutive_failures >= 3 {
                        warn!("3 consecutive tool failures for tree {}", tree_id);
                        emit_error(out, "3 consecutive tool failures, aborting turn".into(), false);
                        emit_event(out, ServerEvent::Done { status: "error".into() });
                        return AgentState::Idle;
                    }
                } else {
                    consecutive_failures = 0;
                }

                emit_event(
                    out,
                    ServerEvent::ToolResult {
                        tool: call.name.clone(),
                        exit: result.exit_code.unwrap_or(0),
                        output: preview_tool_output(&result),
                    },
                );

                if call.name == "bash" {
                    let exit_code = result.exit_code.unwrap_or(0);
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

                let tool_result_msg = make_tool_result_message(&result, call);
                let result_msg_id = agent_core::util::generate_entry_id();
                write_message_entry(store, tree_id, out, &result_msg_id, leaf_id.as_deref(), &tool_result_msg);
                leaf_id = Some(result_msg_id);
                messages.push(tool_result_msg);
            }

            if consecutive_failures >= 3 || tool_call_round >= max_per_turn {
                if tool_call_round >= max_per_turn {
                    warn!(
                        "Max tool call rounds ({}) reached for tree {}",
                        max_per_turn, tree_id
                    );
                    emit_error(
                        out,
                        format!("Max tool call rounds ({}) reached", max_per_turn),
                        false,
                    );
                    emit_event(out, ServerEvent::Done { status: "error".into() });
                }
                return AgentState::Idle;
            }

            let definitions = collect_tool_definitions(tools);
            send_llm_request(out, messages.clone(), definitions);

            AgentState::new_streaming(messages, leaf_id, tool_call_round, tool_calls_this_turn, consecutive_failures)
        }
        "stop" | "length" => {
            emit_event(out, ServerEvent::Done { status: reason.to_string() });

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
        _ => {
            warn!("Unknown finish reason '{}' for tree {}", reason, tree_id);
            emit_event(out, ServerEvent::Done { status: reason.to_string() });
            AgentState::Idle
        }
    }
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

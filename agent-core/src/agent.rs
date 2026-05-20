//! Agent loop and context building.
//!
//! The agent loop:
//!   1. Wait for user message
//!   2. Read entries from store, build context
//!   3. Load context files (AGENTS.md / CLAUDE.md)
//!   4. Estimate token count; emit cap warnings
//!   5. Call provider.stream_chat()
//!   6. Parse chunks, emit events, dispatch tools
//!   7. Check caps; loop back for multi-turn tool calls
//!   8. At hard cap -> break (server handles summarization)

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use log::{error, info, warn};

use crate::config::SessionConfig;
use crate::context_files;
use crate::hooks;
use crate::provider::Provider;
use crate::store::Store;
use crate::tools::{self, Tool};
use crate::types::*;

// ── Context building ──

/// Build a list of Messages for the LLM by walking the parent chain from leaf_id.
///
/// Walk upward from leaf_id:
/// - `Message` -> include as-is
/// - `SessionEnd` with continuation_brief -> inject as system message, stop
/// - `ModelSet` -> track model
/// - `GoalSet` -> track goal
/// - Skip: `session_start`, `label`, `bash_exec`
pub fn build_context(entries: &[Entry], leaf_id: &str) -> Vec<Message> {
    let map: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id(), e)).collect();

    let mut messages: Vec<Message> = Vec::new();
    let mut current: Option<&str> = Some(leaf_id);
    let mut found_goal = None;
    let mut found_model = None;

    while let Some(cid) = current {
        let entry = match map.get(cid) {
            Some(e) => e,
            None => break,
        };

        match entry {
            Entry::Message { message, .. } => {
                messages.push(message.clone());
            }
            Entry::SessionEnd {
                continuation_brief, ..
            } => {
                if let Some(brief) = continuation_brief {
                    if !brief.trim().is_empty() {
                        messages.push(Message {
                            role: MessageRole::System,
                            content: MessageContent::Text(format!(
                                "## Previous Session Continuation\n{}",
                                brief
                            )),
                            tool_calls: None,
                            tool_call_id: None,
                            tool_name: None,
                            usage: None,
                            stop_reason: None,
                            is_error: None,
                        });
                    }
                }
                break;
            }
            Entry::GoalSet { goal, .. } => {
                found_goal = Some(goal.clone());
            }
            Entry::ModelSet { model, .. } => {
                found_model = Some(model.clone());
            }
            _ => {}
        }

        current = entry.parent_id();
    }

    messages.reverse();

    if let Some(goal) = found_goal {
        messages.insert(
            0,
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(format!("## Current Goal\n{}", goal)),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            },
        );
    }

    if let Some(model) = found_model {
        messages.insert(
            0,
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(format!("## Current Model\n{}", model)),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            },
        );
    }

    messages
}

// ── Token estimation ──

/// Estimate tokens for a single string. Uses ~3.5 chars/token average.
pub fn estimate_tokens(content: &str) -> usize {
    (content.len() * 2 + 7) / 7
}

/// Estimate total tokens across all messages in a context.
pub fn estimate_context_tokens(messages: &[Message]) -> usize {
    let mut total = 0;
    for msg in messages {
        match &msg.content {
            MessageContent::Text(s) => total += estimate_tokens(s),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => total += estimate_tokens(text),
                        ContentBlock::ToolCall { arguments, .. } => {
                            total += estimate_tokens(&arguments.to_string());
                        }
                    }
                }
            }
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                total += estimate_tokens(&call.arguments.to_string());
            }
        }
    }
    total
}

// ── System prompt builder ──

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

// ── Tool execution helpers ──

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
        let len = output.content.len().min(2000);
        format!(
            "{}\n\n(Output truncated: was {} bytes)",
            &output.content[..len], output.original_size
        )
    } else {
        output.content.clone()
    }
}

fn preview_tool_output(output: &ToolOutput) -> String {
    if output.truncated {
        let len = output.content.len().min(2000);
        format!("{}... (truncated, was {} bytes)", &output.content[..len], output.original_size)
    } else {
        output.content.clone()
    }
}

// ── Entry ID generation ──

// Uses agent-core/src/util.rs for generate_entry_id

// ── Repo path resolution ──

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

/// Auto-generate a title for a tree by asking the LLM.
pub fn auto_title(
    store: &Store,
    provider: &Provider,
    tree_id: &str,
) -> Result<String, String> {
    let entries = store.read_all_entries(tree_id).map_err(|e| e.to_string())?;
    let meta = store.get_tree(tree_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Tree not found".to_string())?;

    let leaf_id = meta.leaf_id.as_deref().ok_or_else(|| "No entries".to_string())?;
    let mut messages = build_context(&entries, leaf_id);

    let system = Message {
        role: MessageRole::System,
        content: MessageContent::Text(
            "Generate a concise title (6 words or fewer) for this coding conversation. \
             Return ONLY the title text, no quotes, no punctuation, no explanation."
                .to_string(),
        ),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
    };
    messages.insert(0, system);

    let response = provider.chat(&messages, &[])
        .map_err(|e| format!("LLM call failed: {}", e))?;

    let title = response.text.trim().trim_matches('"').to_string();
    if title.is_empty() {
        return Err("Generated empty title".to_string());
    }

    let mut meta = meta;
    meta.title = Some(title.clone());
    store.save_tree_meta(&meta).map_err(|e| e.to_string())?;

    Ok(title)
}

// ── Session end writing ──

fn write_session_end(
    store: &Store,
    tree_id: &str,
    event_tx: &mpsc::Sender<ServerEvent>,
    status: SessionStatus,
    continuation_brief: Option<String>,
) {
    let entry = Entry::SessionEnd {
        id: crate::util::generate_entry_id(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: None,
        status,
        continuation_brief,
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        error!("Failed to write session_end for tree {}: {}", tree_id, e);
    }
    let _ = event_tx.send(ServerEvent::Entry(entry));
}

fn truncate_for_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Write a message entry to the store and emit it as an event.
fn write_message_entry(
    store: &Store,
    tree_id: &str,
    event_tx: &mpsc::Sender<ServerEvent>,
    entry_id: &str,
    parent_id: Option<&str>,
    message: &Message,
    leaf_id: &mut Option<String>,
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
    let _ = event_tx.send(ServerEvent::Entry(entry));
    *leaf_id = Some(entry_id.to_string());
}

// ── Agent loop ──

enum ThinkingSegment {
    Thinking(String),
    Text(String),
}

/// Split a content delta into alternating thinking/text segments.
///
/// `in_thinking` tracks whether the parser is currently inside a `<think>` block
/// across chunk boundaries. Both `<think>` and `</think>` may land in the middle
/// of a chunk or straddle two chunks; the caller preserves `in_thinking` between
/// calls to handle the cross-boundary case.
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

/// Run the agent loop for a single tree.
///
/// This function is spawned in a dedicated thread by the server's lifecycle module.
/// It reads from `input_rx` for user messages, builds context, calls the LLM,
/// dispatches tools, and emits events over `event_tx`.
pub fn run_agent(
    tree_id: &str,
    store: Store,
    provider: Provider,
    session_config: SessionConfig,
    input_rx: mpsc::Receiver<AgentInput>,
    event_tx: mpsc::Sender<ServerEvent>,
    stop: Arc<AtomicBool>,
) {
    crate::logging::AGENT_TREE_ID.set(Some(tree_id.to_string()));
    info!("Agent loop started for tree {}", tree_id);

    let cwd = resolve_repo_path(&store, tree_id);
    let tools = tools::all_tools(&cwd);
    let _session_total_tokens: u64 = 0;

    'main: loop {
        // 1. Wait for user message
        let text = match input_rx.recv() {
            Ok(AgentInput::Message { text }) => text,
            Ok(AgentInput::Stop) => {
                info!("Agent received stop signal for tree {}", tree_id);
                // Worker stays alive; cancel applies only to in-flight work,
                // which the atomic flag above already handled. Drain and wait.
                continue;
            }
            Err(_) => {
                info!("Input channel closed for tree {}, exiting", tree_id);
                break;
            }
        };

        if stop.load(Ordering::Relaxed) {
            info!("Stop flag set for tree {}, exiting", tree_id);
            break;
        }

        // Reset stop flag so a cancel that arrived during idle wait
        // doesn't instantly cancel the next turn.
        stop.store(false, Ordering::Relaxed);

        info!("Processing message for tree {}: {}", tree_id, truncate_for_log(&text, 100));

        // 2. Read entries
        let entries = match store.read_all_entries(tree_id) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to read entries for tree {}: {}", tree_id, e);
                let _ = event_tx.send(ServerEvent::Error {
                    message: format!("Failed to read entries: {}", e),
                    fatal: true,
                });
                break;
            }
        };

        let tree_meta = match store.get_tree(tree_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                error!("Tree {} not found", tree_id);
                break;
            }
            Err(e) => {
                error!("Failed to get tree {}: {}", tree_id, e);
                break;
            }
        };

        let mut leaf_id = tree_meta.leaf_id;

        // 3. Build context
        let leaf_ref = leaf_id.as_deref().unwrap_or("root");
        let mut messages = build_context(&entries, leaf_ref);

        // 4. Append and persist user message
        let user_msg_id = crate::util::generate_entry_id();
        let user_msg_parent = leaf_id.clone();
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

        write_message_entry(
            &store, tree_id, &event_tx,
            &user_msg_id, user_msg_parent.as_deref(),
            &user_msg, &mut leaf_id,
        );

        messages.push(user_msg);

        // 5. Load context files and prepend system prompt
        let ctx_files = context_files::load_context_files(&cwd, store.base_dir());
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

        // 6. Run before-LLM hooks
        if let Err(e) = hooks::run_before_llm_hooks(&mut messages) {
            warn!("Before-LLM hook blocked: {}", e);
            let _ = event_tx.send(ServerEvent::Error {
                message: format!("Hook blocked message: {}", e),
                fatal: false,
            });
            continue;
        }

        // 7. Token estimation and cap checks
        let estimated = estimate_context_tokens(&messages);
        let context_window: usize = 128_000;
        let soft_cap = (session_config.soft_cap_pct as usize) * context_window / 100;
        let hard_cap = (session_config.hard_cap_pct as usize) * context_window / 100;

        info!("Context: est. {} tokens (soft={}, hard={})", estimated, soft_cap, hard_cap);

        if estimated >= soft_cap {
            let pct = (estimated * 100 / context_window) as u8;
            let _ = event_tx.send(ServerEvent::CapWarning {
                level: if estimated >= hard_cap { "hard".into() } else { "soft".into() },
                pct,
            });

            if estimated >= hard_cap {
                warn!("Hard cap reached for tree {} (est. {} tokens)", tree_id, estimated);
                let _ = event_tx.send(ServerEvent::Error {
                    message: "Hard context cap reached. Ending session.".into(),
                    fatal: false,
                });
                write_session_end(&store, tree_id, &event_tx, SessionStatus::Continuing, None);
                break;
            }
        }

        // 8. Get tool definitions
        let definitions: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();

        // 9. Multi-turn tool call loop
        let max_per_turn = session_config.max_tool_calls_per_turn;
        let mut consecutive_failures = 0;
        let mut tool_calls_this_turn = 0;

        let mut tool_call_round = 0;

        'turn: while tool_call_round < max_per_turn && !stop.load(Ordering::Relaxed) {
            // Call provider
            let mut chat_stream = match provider.stream_chat(&messages, &definitions) {
                Ok(s) => s,
                Err(e) => {
                    error!("LLM call failed for tree {}: {}", tree_id, e);
                    let _ = event_tx.send(ServerEvent::Error {
                        message: format!("LLM call failed: {}", e),
                        fatal: true,
                    });
                    break 'main;
                }
            };

            let mut response_text = String::new();
            let mut in_thinking = false;
            let mut tool_calls_buf: Vec<ToolCallBuilder> = Vec::new();
            let mut round_finish_reason: Option<String> = None;

            // Inner stream loop
            'stream: loop {
                if stop.load(Ordering::Relaxed) {
                    // User cancelled — emit Done and persist partial response
                    let _ = event_tx.send(ServerEvent::Done {
                        status: "cancelled".into(),
                    });
                    if !response_text.is_empty() {
                        let msg_id = crate::util::generate_entry_id();
                        let msg_parent = leaf_id.clone();
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
                        write_message_entry(
                            &store, tree_id, &event_tx,
                            &msg_id, msg_parent.as_deref(),
                            &assistant_msg, &mut leaf_id,
                        );
                    }
                    break 'turn;
                }

                let line = match chat_stream.next_line() {
                    Some(l) => l,
                    None => break 'stream,
                };

                // Debug: log raw SSE lines from the LLM provider
                let trimmed_line = line.trim();
                if !trimmed_line.is_empty() && !trimmed_line.starts_with(':') {
                    info!("SSE raw: {}", &trimmed_line[..trimmed_line.len().min(500)]);
                }

                // Skip SSE event separators (blank lines) and comment lines
                if trimmed_line.is_empty() || trimmed_line.starts_with(':') {
                    continue;
                }

                let data = trimmed_line
                    .strip_prefix("data: ")
                    .unwrap_or(trimmed_line);
                if data.is_empty() || data == "[DONE]" {
                    info!("SSE stream ended ([DONE])");
                    break 'stream;
                }

                let chunk: ChatChunk = match serde_json::from_str(data) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Failed to parse SSE chunk: {}", e);
                        continue;
                    }
                };

                // Track usage
                if let Some(u) = &chunk.usage {
                    let _ = u.clone();
                }

                let choice = match chunk.choices.first() {
                    Some(c) => c,
                    None => continue,
                };

                // Text delta (skip empty — reasoning models send empty content
                // chunks with reasoning fields before the actual text)

                // Explicit reasoning field (some providers like DeepSeek / Qwen)
                if let Some(rc) = &choice.delta.reasoning {
                    if !rc.is_empty() {
                        let _ = event_tx.send(ServerEvent::ThinkingChunk { content: rc.clone() });
                    }
                }

                // Content delta — split on  thinking /  response tags
                if let Some(delta) = &choice.delta.content {
                    if !delta.is_empty() {
                        for segment in split_thinking_chunks(delta, &mut in_thinking) {
                            match segment {
                                ThinkingSegment::Thinking(t) => {
                                    let _ = event_tx.send(ServerEvent::ThinkingChunk { content: t });
                                }
                                ThinkingSegment::Text(t) if !t.is_empty() => {
                                    response_text.push_str(&t);
                                    let _ = event_tx.send(ServerEvent::TextChunk {
                                        content: t,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Tool call deltas
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

                // Track finish reason
                if let Some(reason) = &choice.finish_reason {
                    if !reason.is_empty() {
                        round_finish_reason = Some(reason.clone());
                    }
                }
            }

            // Process finish reason
            let reason = round_finish_reason.as_deref().unwrap_or("stop");

            match reason {
                "tool_calls" => {
                    // Execute all collected tool calls
                    let completed_calls: Vec<ToolCall> = tool_calls_buf.iter().map(|b| {
                        ToolCall {
                            id: b.id.clone(),
                            name: b.name.clone(),
                            arguments: serde_json::from_str(&b.arguments)
                                .unwrap_or(serde_json::Value::Null),
                        }
                    }).collect();

                    // Persist the assistant message with tool calls
                    let msg_id = crate::util::generate_entry_id();
                    let msg_parent = leaf_id.clone();

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

                    write_message_entry(
                        &store, tree_id, &event_tx,
                        &msg_id, msg_parent.as_deref(),
                        &assistant_msg, &mut leaf_id,
                    );

                    // Add to local messages for continuation
                    messages.push(assistant_msg);

                    tool_calls_buf.clear();
                    tool_call_round += 1;

                    // Execute each tool
                    for call in &completed_calls {
                        tool_calls_this_turn += 1;
                        if tool_calls_this_turn > max_per_turn {
                            warn!("Max tool calls per turn reached for tree {}", tree_id);
                            break 'turn;
                        }

                        // Run hooks
                        if let Err(e) = hooks::run_tool_call_hooks(&call.name, &call.arguments) {
                            let _ = event_tx.send(ServerEvent::Error {
                                message: format!("Hook blocked tool call: {}", e),
                                fatal: false,
                            });
                            consecutive_failures += 1;
                            continue;
                        }

                        // Emit ToolStart
                        let _ = event_tx.send(ServerEvent::ToolStart {
                            tool: call.name.clone(),
                            input: call.arguments.clone(),
                        });

                        // Execute
                        let result = execute_tool(&tools, &call.name, &call.arguments, &stop);

                        if result.exit_code.unwrap_or(0) != 0 {
                            consecutive_failures += 1;
                            if consecutive_failures >= 3 {
                                warn!("3 consecutive tool failures for tree {}", tree_id);
                                let _ = event_tx.send(ServerEvent::Error {
                                    message: "3 consecutive tool failures, aborting turn".into(),
                                    fatal: false,
                                });
                                break 'turn;
                            }
                        } else {
                            consecutive_failures = 0;
                        }

                        // Emit ToolResult
                        let _ = event_tx.send(ServerEvent::ToolResult {
                            tool: call.name.clone(),
                            exit: result.exit_code.unwrap_or(0),
                            output: preview_tool_output(&result),
                        });

                        // Persist BashExec if applicable
                        if call.name == "bash" {
                            let exit_code = result.exit_code.unwrap_or(0);
                            let bash_entry = Entry::BashExec {
                                id: crate::util::generate_entry_id(),
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
                            let _ = event_tx.send(ServerEvent::Entry(bash_entry));
                        }

                        // Add tool result to message context
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

                    // Continue to next LLM call with tool results in context
                    continue 'turn;
                }
                "stop" | "length" => {
                    let _ = event_tx.send(ServerEvent::Done {
                        status: reason.to_string(),
                    });

                    // Persist the assistant message
                    if !response_text.is_empty() {
                        let msg_id = crate::util::generate_entry_id();
                        let msg_parent = leaf_id.clone();
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

                        write_message_entry(
                            &store, tree_id, &event_tx,
                            &msg_id, msg_parent.as_deref(),
                            &assistant_msg, &mut leaf_id,
                        );
                    }

                    // Update tree metadata
                    if let Ok(Some(mut meta)) = store.get_tree(tree_id) {
                        meta.leaf_id = leaf_id.clone();
                        meta.updated_at = chrono::Utc::now().timestamp();
                        let _ = store.save_tree_meta(&meta);
                    }

                    break 'turn;
                }
                _ => {
                    // Unknown finish reason, try to handle gracefully
                    warn!("Unknown finish reason '{}' for tree {}", reason, tree_id);
                    let _ = event_tx.send(ServerEvent::Done {
                        status: reason.to_string(),
                    });
                    break 'turn;
                }
            }
        }

        if tool_call_round >= max_per_turn {
            warn!("Max tool call rounds ({}) reached for tree {}", max_per_turn, tree_id);
            let _ = event_tx.send(ServerEvent::Error {
                message: format!("Max tool call rounds ({}) reached", max_per_turn),
                fatal: false,
            });
        }

        // If the turn was cancelled (stop flag set during tool execution,
        // not during streaming which already emits Done above), emit Done.
        if stop.load(Ordering::Relaxed) {
            let _ = event_tx.send(ServerEvent::Done {
                status: "cancelled".into(),
            });
        }
    }

    info!("Agent loop exiting for tree {}", tree_id);
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(id: &str, parent: Option<&str>, role: MessageRole, text: &str) -> Entry {
        Entry::Message {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            message: Message {
                role,
                content: MessageContent::Text(text.to_string()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            },
        }
    }

    fn make_session_start(id: &str, parent: Option<&str>) -> Entry {
        Entry::SessionStart {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn make_session_end(id: &str, parent: Option<&str>, brief: Option<&str>) -> Entry {
        Entry::SessionEnd {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            summary: None,
            status: SessionStatus::Continuing,
            continuation_brief: brief.map(|s| s.to_string()),
        }
    }

    fn make_goal_set(id: &str, parent: Option<&str>, goal: &str) -> Entry {
        Entry::GoalSet {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            goal: goal.to_string(),
        }
    }

    fn make_model_set(id: &str, parent: Option<&str>, model: &str) -> Entry {
        Entry::ModelSet {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn test_build_context_empty_tree() {
        let entries = vec![make_session_start("1", None)];
        let messages = build_context(&entries, "1");
        assert!(messages.is_empty(), "SessionStart should be excluded");
    }

    #[test]
    fn test_build_context_single_message() {
        let entries = vec![
            make_session_start("root", None),
            make_message("msg1", Some("root"), MessageRole::User, "hello"),
        ];
        let messages = build_context(&entries, "msg1");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::User);
    }

    fn message_text(msg: &Message) -> &str {
        match &msg.content {
            MessageContent::Text(t) => t,
            _ => "",
        }
    }

    #[test]
    fn test_build_context_chain() {
        let entries = vec![
            make_session_start("root", None),
            make_message("msg1", Some("root"), MessageRole::User, "first"),
            make_message("msg2", Some("msg1"), MessageRole::Assistant, "second"),
            make_message("msg3", Some("msg2"), MessageRole::User, "third"),
        ];
        let messages = build_context(&entries, "msg3");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[2].role, MessageRole::User);
    }

    #[test]
    fn test_build_context_with_continuation_brief() {
        let entries = vec![
            make_session_start("root", None),
            make_session_end("end1", Some("root"), Some("Accomplished X")),
            make_session_start("ss2", Some("end1")),
            make_message("msg1", Some("ss2"), MessageRole::User, "continue"),
        ];
        let messages = build_context(&entries, "msg1");
        assert!(messages.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("Accomplished X"))
        }));
    }

    #[test]
    fn test_build_context_with_goal() {
        let entries = vec![
            make_session_start("root", None),
            make_goal_set("goal1", Some("root"), "Refactor auth"),
            make_message("msg1", Some("goal1"), MessageRole::User, "start"),
        ];
        let messages = build_context(&entries, "msg1");
        assert!(messages.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("Refactor auth"))
        }));
    }

    #[test]
    fn test_build_context_with_model() {
        let entries = vec![
            make_session_start("root", None),
            make_model_set("mod1", Some("root"), "claude-3.5"),
            make_message("msg1", Some("mod1"), MessageRole::User, "hello"),
        ];
        let messages = build_context(&entries, "msg1");
        assert!(messages.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("claude-3.5"))
        }));
    }

    #[test]
    fn test_estimate_tokens_short() {
        let est = estimate_tokens("hello world");
        assert!(est > 0);
        assert!(est <= 10);
    }

    #[test]
    fn test_estimate_tokens_long() {
        let text = "a".repeat(1000);
        let est = estimate_tokens(&text);
        assert!(est > 200);
        assert!(est <= 400);
    }

    #[test]
    fn test_estimate_context_tokens_empty() {
        assert_eq!(estimate_context_tokens(&[]), 0);
    }

    #[test]
    fn test_estimate_context_tokens_single() {
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
        };
        let est = estimate_context_tokens(&[msg]);
        assert_eq!(est, estimate_tokens("hello"));
    }

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
//! Interactive TUI loop for the agent CLI.
//!
//! Uses a single `tokio::select!` over:
//! - `crossterm::event::EventStream` (terminal input)
//! - `WebSocketStream::next()` (incoming server events)
//! - 16 ms render tick (ratatui draw)
//!
//! No `nix::poll`, no blocking I/O, no explicit fd tracking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use ratatui::text::{Line, Span};
use tokio_tungstenite::tungstenite::Message;

use agent_core::types::{
    ContextStatus, Entry, MessageContent, MessageRole, NotificationLevel, ServerEvent,
};

use crate::app::{AppMode, AppState, CreateTreeStep, HistoryItem};
use crate::markdown::MarkdownEmitter;
use crate::tui::{App, AppEvent};
use crate::Backend;

pub const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

// ── Persistent token / cost counters ─────────────────────────────────────

#[derive(Default, Clone)]
pub struct PersistentCounters {
    pub model: Option<String>,
    pub input_rate: f64,
    pub output_rate: f64,
    pub cum_prompt_tokens: u64,
    pub cum_completion_tokens: u64,
    pub cum_cached_tokens: u64,
    pub cache_supported: bool,
    pub last_turn_cache_pct: Option<u8>,
}

fn model_pricing(model: &str) -> (f64, f64) {
    if model.contains("deepseek") || model.contains("gpt-4o-mini") {
        (0.15e-6, 0.60e-6)
    } else if model.contains("claude") {
        (3.00e-6, 15.00e-6)
    } else if model.contains("gpt-4") {
        (10.00e-6, 30.00e-6)
    } else {
        (0.15e-6, 0.60e-6)
    }
}

// ── format_tool_args ──────────────────────────────────────────────────────

fn format_tool_args(tool: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let pick = match tool {
        "bash" => obj.get("command"),
        "read" => {
            let path = obj.get("file_path").or_else(|| obj.get("path"))
                .and_then(|v| v.as_str()).unwrap_or("");
            let offset = obj.get("offset").and_then(|v| v.as_i64());
            let limit = obj.get("limit").and_then(|v| v.as_i64());
            return match (offset, limit) {
                (Some(o), Some(l)) => format!("{path}  {o}–{}", o + l - 1),
                (Some(o), None) => format!("{path}  {o}–"),
                (None, Some(l)) => format!("{path}  1–{l}"),
                (None, None) => path.to_string(),
            };
        }
        "write" | "edit" => obj.get("file_path").or_else(|| obj.get("path")),
        "find" => obj.get("pattern").or_else(|| obj.get("path")),
        "grep" => obj.get("pattern"),
        "git" => obj.get("command").or_else(|| obj.get("args")),
        "search_messages" => obj.get("query"),
        "restore_edit" => {
            let id = obj.get("id").and_then(|v| v.as_i64()).map(|n| n.to_string());
            let mode = obj.get("mode").and_then(|v| v.as_str()).map(|s| s.to_string());
            return match (id, mode) {
                (Some(id), Some(mode)) => format!("{id}  {mode}"),
                (Some(id), None) => id,
                (None, Some(mode)) => mode,
                (None, None) => String::new(),
            };
        }
        _ => None,
    };
    match pick.and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let raw = serde_json::to_string(input).unwrap_or_default();
            if raw.len() > 80 { format!("{}…", &raw[..80]) } else { raw }
        }
    }
}

// ── Status bar rendering ──────────────────────────────────────────────────

fn build_status(state: &mut AppState, persistent: &PersistentCounters) {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("🤖 {}", persistent.model.as_deref().unwrap_or("—")));
    let _ctx_pct = persistent.last_turn_cache_pct.unwrap_or(0);
    parts.push(format!("💰 ${:.4}", cost(persistent)));
    let session_rate = if persistent.cum_prompt_tokens > 0 {
        persistent.cum_cached_tokens as f64 / persistent.cum_prompt_tokens as f64 * 100.0
    } else {
        0.0
    };
    match (persistent.cache_supported, persistent.last_turn_cache_pct) {
        (true, Some(last)) => parts.push(format!("💾 {:.0}% (last {:.0}%)", session_rate, last)),
        (true, None) => parts.push(format!("💾 {:.0}%", session_rate)),
        (false, _) => parts.push("💾 ?".into()),
    }
    let text = parts.join("  ");
    state.status = Line::from(vec![Span::raw(text)]);
}

fn cost(p: &PersistentCounters) -> f64 {
    p.cum_prompt_tokens as f64 * p.input_rate + p.cum_completion_tokens as f64 * p.output_rate
}

// ── Event application ─────────────────────────────────────────────────────

fn apply_event(
    event: &ServerEvent,
    state: &mut AppState,
    streaming: &mut StreamingState,
    persistent: &mut PersistentCounters,
    width: u16,
) {
    match event {
        ServerEvent::TextChunk { content } => {
            let active = state.active_or_new();
            if active.in_thinking {
                active.in_thinking = false;
            }
            active.content_text.push_str(content);
            let active_ptr = state.active.as_mut().unwrap();
            let _ = streaming.md.push(content, &mut |spans| {
                active_ptr.push_content_spans(spans);
                Ok(())
            }, width as usize);
        }

        ServerEvent::ThinkingChunk { content } => {
            let active = state.active_or_new();
            active.in_thinking = true;
            active.thinking_text.push_str(content);
            active.push_thinking_chunk(content);
        }

        ServerEvent::ToolStart { tool, input } => {
            streaming.last_tool_args = Some((tool.clone(), input.clone()));
        }

        ServerEvent::ToolResult { tool, exit, output } => {
            if state.active.is_some() {
                let active = state.active.as_mut().unwrap();
                let _ = streaming.md.flush(&mut |spans| {
                    active.push_content_spans(spans);
                    Ok(())
                }, width as usize);
                state.finalize_active();
            }
            let args = streaming.last_tool_args.take()
                .map(|(_, input)| format_tool_args(tool, &input))
                .unwrap_or_default();
            state.push_history(HistoryItem::ToolResult {
                tool: tool.clone(),
                args,
                output: output.clone(),
                exit: *exit,
            });
        }

        ServerEvent::Entry(entry) => {
            apply_entry(entry, state);
        }

        ServerEvent::Notification { level, message } => {
            if state.active.is_some() {
                let active = state.active.as_mut().unwrap();
                let _ = streaming.md.flush(&mut |spans| {
                    active.push_content_spans(spans);
                    Ok(())
                }, width as usize);
            }
            state.push_history(HistoryItem::Notification {
                level: level.clone(),
                message: message.clone(),
            });
        }

        ServerEvent::Diagnostics { source, files } => {
            state.push_history(HistoryItem::Diagnostics {
                source: source.clone(),
                files: files.clone(),
            });
        }

        ServerEvent::ContextUpdate { status, pct, estimated: _ } => {
            match status {
                ContextStatus::Warning | ContextStatus::Critical => {
                    state.push_history(HistoryItem::Notification {
                        level: NotificationLevel::Warning,
                        message: format!("Context at {}% ({:?})", pct, status),
                    });
                }
                _ => {}
            }
        }

        ServerEvent::Done { status, usage } => {
            if let Some(u) = usage {
                persistent.cum_prompt_tokens += u.prompt_tokens;
                persistent.cum_completion_tokens += u.completion_tokens;
                let turn_cached = u.cached_prompt_tokens.unwrap_or(0);
                if u.prompt_tokens > 0 {
                    persistent.last_turn_cache_pct =
                        Some((turn_cached as f64 / u.prompt_tokens as f64 * 100.0).round() as u8);
                }
                if let Some(cached) = u.cached_prompt_tokens {
                    persistent.cum_cached_tokens += cached;
                    persistent.cache_supported = true;
                } else if persistent.cache_supported && persistent.cum_prompt_tokens > 0 {
                    let prev = persistent.cum_prompt_tokens - u.prompt_tokens;
                    let rate = if prev > 0 { persistent.cum_cached_tokens as f64 / prev as f64 } else { 0.0 };
                    persistent.cum_cached_tokens += (u.prompt_tokens as f64 * rate).round() as u64;
                }
            }

            if state.active.is_some() {
                let active = state.active.as_mut().unwrap();
                let _ = streaming.md.flush(&mut |spans| {
                    active.push_content_spans(spans);
                    Ok(())
                }, width as usize);
                if !active.partial_line.is_empty() {
                    let line = ratatui::text::Line::from(std::mem::take(&mut active.partial_line));
                    active.content_lines.push(line);
                }
                state.finalize_active();
            }

            let status_item = match status.as_str() {
                "stop" | "complete" | "error" | "history" => None,
                "length" => Some(HistoryItem::Notification {
                    level: NotificationLevel::Warning,
                    message: "Stopped at length limit".into(),
                }),
                "aborted" => Some(HistoryItem::Notification {
                    level: NotificationLevel::Warning,
                    message: "✖ Aborted".into(),
                }),
                "cancelled" => Some(HistoryItem::Notification {
                    level: NotificationLevel::Info,
                    message: "✋ Cancelled".into(),
                }),
                other => Some(HistoryItem::Notification {
                    level: NotificationLevel::Warning,
                    message: format!("Unknown completion status: {}", other),
                }),
            };
            if let Some(item) = status_item {
                state.push_history(item);
            }
        }

        ServerEvent::FileChanged { path, kind } => {
            state.push_history(HistoryItem::FileChanged {
                path: path.clone(),
                kind: kind.clone(),
            });
        }

        ServerEvent::MetaUpdate { title, model } => {
            if let Some(t) = title {
                state.push_history(HistoryItem::Info(format!("Title: {}", t)));
            }
            if let Some(m) = model {
                persistent.model = Some(m.clone());
                let (ir, or) = model_pricing(m);
                persistent.input_rate = ir;
                persistent.output_rate = or;
            }
        }
    }

    build_status(state, persistent);
}

fn apply_entry(entry: &Entry, state: &mut AppState) {
    match entry {
        Entry::Message { message, .. } if message.role == MessageRole::User => {
            let text = match &message.content {
                MessageContent::Text(t) => t.clone(),
                _ => "[content blocks]".into(),
            };
            // Skip duplicate: the Submit handler already pushed this locally.
            let is_dup = state.history.last()
                .map(|item| matches!(item, HistoryItem::User(t) if t == &text))
                .unwrap_or(false);
            if !is_dup {
                state.push_history(HistoryItem::User(text));
            }
        }

        Entry::Message { message, .. } => {
            let thinking = message.thinking.clone().unwrap_or_default();
            let content = match &message.content {
                MessageContent::Text(t) => t.clone(),
                _ => "[content blocks]".into(),
            };
            if !content.is_empty() || !thinking.is_empty() {
                state.push_history(HistoryItem::Assistant { content, thinking });
            }
        }

        Entry::GoalSet { goal, .. } => {
            state.push_history(HistoryItem::Info(format!("🎯  {}", goal)));
        }

        Entry::ModelSet { model, .. } => {
            state.push_history(HistoryItem::Info(format!("🤖  Model: {}", model)));
        }

        Entry::SessionEnd { summary, status, .. } => {
            let s = summary.as_deref().unwrap_or("");
            state.push_history(HistoryItem::SessionEnd {
                status: format!("{:?}", status),
                summary: s.to_string(),
            });
        }

        Entry::BashExec { command, output, exit_code, .. } => {
            state.push_history(HistoryItem::ToolResult {
                tool: "bash".into(),
                args: command.clone(),
                output: output.clone(),
                exit: *exit_code,
            });
        }

        _ => {}
    }
}

// ── Streaming session state ───────────────────────────────────────────────

struct StreamingState {
    last_tool_args: Option<(String, serde_json::Value)>,
    last_spinner: Instant,
    md: MarkdownEmitter,
}

impl StreamingState {
    fn new() -> Self {
        Self {
            last_tool_args: None,
            last_spinner: Instant::now(),
            md: MarkdownEmitter::new(),
        }
    }
}

// ── Navigation event handling ─────────────────────────────────────────────

/// Handles scroll/toggle/resize events. Returns true if the event was consumed.
fn handle_nav_event(event: AppEvent, state: &mut AppState, app: &mut App) -> Result<bool, String> {
    match event {
        AppEvent::ScrollUp(n) => state.scroll_offset += n as usize,
        AppEvent::ScrollDown(n) => state.scroll_offset = state.scroll_offset.saturating_sub(n as usize),
        AppEvent::ScrollToTop => state.scroll_offset = usize::MAX,
        AppEvent::ScrollToBottom => state.scroll_to_bottom(),
        AppEvent::ToggleThinking => {
            state.show_thinking ^= true;
            state.suppress_scroll_compensation = true;
        }
        AppEvent::Resize => {
            for cache in &mut state.cache { cache.width = 0; }
            if let Some(active) = &mut state.active { active.rendered_width = 0; }
            state.suppress_scroll_compensation = true;
        }
        _ => return Ok(false),
    }
    app.draw(state).map_err(|e| e.to_string())?;
    Ok(true)
}

// ── Tree selection ────────────────────────────────────────────────────────

async fn select_or_create_tree(
    app: &mut App,
    state: &mut AppState,
    backend: &Backend,
) -> Result<String, String> {
    loop {
        let trees = backend.list_trees().await.map_err(|e| e.to_string())?;

        if trees.is_empty() {
            return create_tree_interactive(app, state, backend).await;
        }

        state.mode = AppMode::SelectTree { trees: trees.clone(), selected: 0 };
        app.draw(state).map_err(|e| e.to_string())?;

        loop {
            match app.poll_event_blocking(state, Duration::from_millis(16))
                .map_err(|e| e.to_string())?
            {
                Some(AppEvent::SelectUp) => {
                    if let AppMode::SelectTree { ref mut selected, .. } = state.mode {
                        if *selected > 0 { *selected -= 1; }
                    }
                    app.draw(state).map_err(|e| e.to_string())?;
                }
                Some(AppEvent::SelectDown) => {
                    if let AppMode::SelectTree { ref mut selected, ref trees } = state.mode {
                        if *selected + 1 < trees.len() { *selected += 1; }
                    }
                    app.draw(state).map_err(|e| e.to_string())?;
                }
                Some(AppEvent::Confirm) => {
                    if let AppMode::SelectTree { ref trees, selected } = state.mode {
                        let id = trees[selected].id.clone();
                        state.mode = AppMode::Chat;
                        return Ok(id);
                    }
                }
                Some(AppEvent::NewTree) => {
                    state.mode = AppMode::Chat;
                    return create_tree_interactive(app, state, backend).await;
                }
                Some(AppEvent::Cancel) => {
                    return Err("quit".into());
                }
                _ => {}
            }
        }
    }
}

async fn create_tree_interactive(
    app: &mut App,
    state: &mut AppState,
    backend: &Backend,
) -> Result<String, String> {
    let steps = [CreateTreeStep::Title, CreateTreeStep::RepoPath, CreateTreeStep::Model];
    let mut title = String::new();
    let mut repo_path = String::new();
    let mut model_input = String::new();

    for &step in &steps {
        state.mode = AppMode::CreateTree {
            step,
            title: title.clone(),
            repo_path: repo_path.clone(),
            model: model_input.clone(),
        };

        app.textarea = tui_textarea::TextArea::default();
        app.textarea.set_cursor_line_style(ratatui::style::Style::default());

        app.draw(state).map_err(|e| e.to_string())?;

        let value = loop {
            match app.poll_event_blocking(state, Duration::from_millis(16))
                .map_err(|e| e.to_string())?
            {
                Some(AppEvent::Confirm) => {
                    let text = app.textarea.lines().join("\n");
                    break text.trim().to_string();
                }
                Some(AppEvent::Cancel) => return Err("quit".into()),
                _ => {
                    app.draw(state).map_err(|e| e.to_string())?;
                }
            }
        };

        match step {
            CreateTreeStep::Title => {
                title = if value.is_empty() { "default".into() } else { value };
            }
            CreateTreeStep::RepoPath => {
                repo_path = value;
            }
            CreateTreeStep::Model => {
                model_input = value;
            }
        }
    }

    let meta = backend.create_tree(
        Some(&title),
        if repo_path.is_empty() { None } else { Some(&repo_path) },
        if model_input.is_empty() { None } else { Some(&model_input) },
        &[],
        None,
        &[],
        &[],
    ).await.map_err(|e| e.to_string())?;

    state.mode = AppMode::Chat;
    let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    state.push_history(HistoryItem::Info(format!(
        "Created tree {} ({})", short_id, meta.title.as_deref().unwrap_or("untitled"),
    )));
    state.scroll_to_bottom();

    Ok(meta.id)
}

// ── Main entry point ──────────────────────────────────────────────────────

pub async fn run_interactive(
    backend: &Backend,
    initial_repo_path: Option<String>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut app = App::new().map_err(|e| format!("terminal init: {}", e))?;
    let mut state = AppState::new();
    let mut persistent = PersistentCounters::default();
    let mut input_history: Vec<String> = Vec::new();
    let mut history_idx: Option<usize> = None;
    let mut history_draft = String::new();

    build_status(&mut state, &persistent);
    app.draw(&mut state).map_err(|e| e.to_string())?;

    let current_tree_id = if let Some(rp) = initial_repo_path {
        let abs = std::path::Path::new(&rp);
        let abs = if abs.is_relative() {
            std::env::current_dir().unwrap_or_default().join(&rp)
        } else { abs.to_path_buf() };
        let rp_str = abs.to_string_lossy().to_string();

        let meta = backend.create_tree(Some("untitled"), Some(&rp_str), None, &[], None, &[], &[])
            .await
            .map_err(|e| format!("failed to create tree: {}", e))?;
        let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
        state.push_history(HistoryItem::Info(format!("Created tree {} in {}", short_id, rp_str)));
        state.scroll_to_bottom();
        app.draw(&mut state).map_err(|e| e.to_string())?;
        meta.id
    } else {
        match select_or_create_tree(&mut app, &mut state, backend).await {
            Ok(id) => id,
            Err(e) if e == "quit" => {
                app.teardown().ok();
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    };

    // Show tree info
    match backend.get_tree(&current_tree_id).await {
        Ok(meta) => {
            let title = meta.title.as_deref().unwrap_or("untitled");
            let short_id = if current_tree_id.len() > 8 { &current_tree_id[..8] } else { &current_tree_id };
            state.push_history(HistoryItem::Info(format!("{}  ·  {}", title, short_id)));
            state.scroll_to_bottom();
        }
        Err(e) => {
            state.push_history(HistoryItem::Notification {
                level: NotificationLevel::Warning,
                message: format!("Failed to load tree: {}", e),
            });
            state.scroll_to_bottom();
        }
    }

    // ── Establish persistent WebSocket connection ────────────────────────
    //
    // We keep one WS connection alive for the entire interactive session.
    // Wrapped in Arc<Mutex<>> so both the terminal handler (for sending)
    // and the select! WS branch (for receiving) can access it. We do NOT
    // split the stream — session.next_event() handles ping/pong internally
    // and is cancel-safe, so it works directly in select!.

    let mut session = backend
        .connect_session(&current_tree_id)
        .await
        .map_err(|e| format!("connect session: {}", e))?;

    // Replay history: drain the initial `GetEntries` replay stream.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut replay_state = StreamingState::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), session.next_event()).await {
            Ok(Some(Ok(ServerEvent::Done { status, .. }))) if status == "history" => break,
            Ok(Some(Ok(ev))) => {
                apply_event(&ev, &mut state, &mut replay_state, &mut persistent, 80);
            }
            Ok(Some(Err(e))) => {
                eprintln!("Replay parse error: {}", e);
                break;
            }
            Ok(None) => break,  // WS closed
            Err(_) => {}        // timeout, loop
        }
    }
    app.draw(&mut state).map_err(|e| e.to_string())?;

    // Wrap session in Arc<Mutex> for shared access between branches.
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(session));
    use tokio_tungstenite::tungstenite::Message;

    // ── "Are we waiting for a response?" state ───────────────────────────
    let mut streaming: Option<StreamingState> = None;
    let mut ws_open = true;

    // ── Terminal event stream ────────────────────────────────────────────
    let mut term = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16));

    loop {
        // Check for external stop signal (Ctrl+C from the binary-level handler).
        if stop.load(Ordering::Relaxed) {
            if streaming.is_some() {
                // Send stop to the worker.
                let cmd = serde_json::to_string(&agent_core::rpc::WsCommand::Stop).unwrap();
                let mut guard = session.lock().await;
                let _ = guard.ws.send(Message::Text(cmd.into())).await;
            } else {
                break; // idle + Ctrl+C → quit
            }
        }

        if !ws_open {
            break;
        }

        let width = app.width();

        // ── Single select! over all event sources ────────────────────────
        tokio::select! {
            biased; // prioritise terminal input for snappy UI

            // Terminal events (keyboard, mouse, resize)
            term_event = term.next() => {
                match term_event {
                    Some(Ok(ev)) => {
                        let mut consumed = false;

                        // Try navigation/history events first (they apply immediately).
                        if let Some(app_event) = app.handle_chat_event_raw(&ev) {
                            let is_submit = matches!(&app_event, AppEvent::Submit(_));
                            consumed = handle_nav_event(app_event.clone(), &mut state, &mut app)
                                .unwrap_or(false);

                            if is_submit && streaming.is_none() {
                                // User pressed Enter — get the text and send it.
                                let text = app.textarea.lines().join("\n");
                                let text = text.trim().to_string();
                                if !text.is_empty() {
                                    if input_history.last().map(|s| s.as_str()) != Some(&text) {
                                        input_history.push(text.clone());
                                    }
                                    history_idx = None;

                                    state.push_history(HistoryItem::User(text.clone()));
                                    state.scroll_to_bottom();

                                    // Send via WS.
                                    let cmd = agent_core::rpc::WsCommand::Message {
                                        params: agent_core::rpc::MessageParams { text: text.clone() },
                                    };
                                    let s = serde_json::to_string(&cmd).unwrap();
                                    let mut guard = session.lock().await;
                                    if guard.ws.send(Message::Text(s.into())).await.is_err() {
                                        ws_open = false;
                                        break;
                                    }

                                    state.spinner_active = true;
                                    streaming = Some(StreamingState::new());
                                    build_status(&mut state, &mut persistent);

                                    // Clear textarea.
                                    app.textarea = tui_textarea::TextArea::default();
                                    app.textarea.set_cursor_line_style(ratatui::style::Style::default());
                                    app.textarea.set_placeholder_text("Type a message… (Enter to send, Shift+Enter for newline)");
                                }
                            } else if matches!(&app_event, AppEvent::Cancel) {
                                if streaming.is_some() {
                                    // Send stop command.
                                    let cmd = serde_json::to_string(&agent_core::rpc::WsCommand::Stop).unwrap();
                                    let mut guard = session.lock().await;
                                    let _ = guard.ws.send(Message::Text(cmd.into())).await;
                                    state.push_history(HistoryItem::Notification {
                                        level: NotificationLevel::Info,
                                        message: "⏸ Cancelling…".into(),
                                    });
                                    state.scroll_to_bottom();
                                } else {
                                    // Idle Ctrl+C → quit.
                                    break;
                                }
                            }
                        }

                        if !consumed {
                            // Let the textarea process the key.
                            app.handle_textarea_input(&ev);
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("Terminal event error: {}", e);
                        break;
                    }
                    None => break,
                }
            }

            // WebSocket events (server responses)
            // session.next_event() handles ping/pong internally and is cancel-safe.
            ws_event = async { session.lock().await.next_event().await } => {
                match ws_event {
                    Some(Ok(ev)) => {
                        let is_done = matches!(&ev, ServerEvent::Done { .. });
                        let is_fatal = matches!(&ev, ServerEvent::Notification {
                            level: NotificationLevel::Fatal, ..
                        });

                        // Skip replay entries in the live stream (applied by replay above).
                        if !matches!(&ev, ServerEvent::Done { status, .. } if status == "history") {
                            if let Some(ref mut s) = streaming {
                                apply_event(&ev, &mut state, s, &mut persistent, width);
                            } else {
                                // Events arriving without an active turn (e.g. MetaUpdate)
                                let mut dummy = StreamingState::new();
                                apply_event(&ev, &mut state, &mut dummy, &mut persistent, width);
                            }
                        }

                        if is_done {
                            streaming = None;
                            state.spinner_active = false;
                            build_status(&mut state, &mut persistent);
                        } else if is_fatal {
                            streaming = None;
                            state.spinner_active = false;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("WS error: {}", e);
                        ws_open = false;
                    }
                    None => {
                        ws_open = false;
                    }
                }
            }

            // Render tick (60 fps)
            _ = tick.tick() => {
                // Spinner advance
                if let Some(ref mut s) = streaming {
                    if s.last_spinner.elapsed() >= SPINNER_INTERVAL {
                        state.spinner_frame = (state.spinner_frame + 1) % 10;
                        s.last_spinner = Instant::now();
                    }
                }
                let _ = app.draw(&mut state);
            }
        }
    }

    app.teardown().ok();
    Ok(())
}

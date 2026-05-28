//! Interactive TUI loop for the agent CLI.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::text::{Line, Span};

use agent_core::types::{
    ContextStatus, Entry, MessageContent, MessageRole, NotificationLevel, ServerEvent,
};

use crate::app::{AppMode, AppState, CreateTreeStep, HistoryItem};
use crate::client::{AgentSession, TryEvent};
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
            state.push_history(HistoryItem::User(text));
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

// ── Blocking wait on WS fds + stdin ──────────────────────────────────────

#[cfg(unix)]
fn wait_for_event(ws_fds: &[RawFd], timeout: Duration) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::unix::io::BorrowedFd;
    let pt = PollTimeout::from(timeout.as_millis().min(u16::MAX as u128) as u16);
    // Poll WS fds + stdin so we wake on either network data or a keypress.
    let all_fds: Vec<RawFd> = std::iter::once(0).chain(ws_fds.iter().copied()).collect();
    let mut pfds: Vec<PollFd> = all_fds.iter()
        .map(|&fd| unsafe { PollFd::new(BorrowedFd::borrow_raw(fd), PollFlags::POLLIN) })
        .collect();
    let _ = poll(&mut pfds, pt);
}

#[cfg(not(unix))]
fn wait_for_event(_ws_fds: &[RawFd], timeout: Duration) {
    std::thread::sleep(timeout);
}

// ── Streaming session state ───────────────────────────────────────────────

struct StreamingState {
    session: AgentSession,
    ws_fds: Vec<RawFd>,
    cancel_signalled: bool,
    last_tool_args: Option<(String, serde_json::Value)>,
    last_spinner: Instant,
    md: MarkdownEmitter,
}

impl StreamingState {
    fn start(backend: &Backend, tree_id: &str, text: &str) -> Result<Self, String> {
        let mut session = backend.connect_session(tree_id).map_err(|e| e.to_string())?;
        session.set_nonblocking(true)?;
        session.send_message(text).map_err(|e| e.to_string())?;
        let ws_fds = session.as_raw_fd().into_iter().collect();
        Ok(Self {
            session,
            ws_fds,
            cancel_signalled: false,
            last_tool_args: None,
            last_spinner: Instant::now(),
            md: MarkdownEmitter::new(),
        })
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

// ── History replay ────────────────────────────────────────────────────────

fn replay_entries(
    backend: &Backend,
    tree_id: &str,
    state: &mut AppState,
    persistent: &mut PersistentCounters,
) -> Result<(), String> {
    let mut session = backend.connect_session(tree_id).map_err(|e| e.to_string())?;
    session.set_nonblocking(true).map_err(|e| e.to_string())?;
    let ws_fds: Vec<RawFd> = session.as_raw_fd().into_iter().collect();
    let mut dummy_streaming = StreamingState {
        session,
        ws_fds: ws_fds.clone(),
        cancel_signalled: false,
        last_tool_args: None,
        last_spinner: Instant::now(),
        md: MarkdownEmitter::new(),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);

    loop {
        loop {
            match dummy_streaming.session.try_next_event() {
                TryEvent::Event(ServerEvent::Done { status, .. }) if status == "history" => {
                    return Ok(());
                }
                TryEvent::Event(ev) => {
                    // Use a dummy width for replay; cache will be re-rendered on first draw.
                    apply_event(&ev, state, &mut dummy_streaming, persistent, 80);
                }
                TryEvent::WouldBlock => break,
                TryEvent::Closed | TryEvent::Err(_) => return Ok(()),
            }
        }

        if std::time::Instant::now() >= deadline { break; }

        let ms = deadline.saturating_duration_since(std::time::Instant::now()).as_millis().min(200) as u64;
        wait_for_event(&ws_fds, Duration::from_millis(ms));
    }
    Ok(())
}

// ── Tree selection ────────────────────────────────────────────────────────

fn select_or_create_tree(
    app: &mut App,
    state: &mut AppState,
    backend: &Backend,
) -> Result<String, String> {
    loop {
        let trees = backend.list_trees().map_err(|e| e.to_string())?;

        if trees.is_empty() {
            return create_tree_interactive(app, state, backend);
        }

        state.mode = AppMode::SelectTree { trees: trees.clone(), selected: 0 };
        app.draw(state).map_err(|e| e.to_string())?;

        loop {
            match app.poll_event(state, Duration::from_millis(16))
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
                    return create_tree_interactive(app, state, backend);
                }
                Some(AppEvent::Cancel) => {
                    return Err("quit".into());
                }
                _ => {}
            }
        }
    }
}

fn create_tree_interactive(
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
            match app.poll_event(state, Duration::from_millis(16))
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
    ).map_err(|e| e.to_string())?;

    state.mode = AppMode::Chat;
    let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    state.push_history(HistoryItem::Info(format!(
        "Created tree {} ({})", short_id, meta.title.as_deref().unwrap_or("untitled")
    )));
    state.scroll_to_bottom();

    Ok(meta.id)
}

// ── Main entry point ──────────────────────────────────────────────────────

pub fn run_interactive(
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
            .map_err(|e| format!("failed to create tree: {}", e))?;
        let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
        state.push_history(HistoryItem::Info(format!("Created tree {} in {}", short_id, rp_str)));
        state.scroll_to_bottom();
        app.draw(&mut state).map_err(|e| e.to_string())?;
        meta.id
    } else {
        match select_or_create_tree(&mut app, &mut state, backend) {
            Ok(id) => id,
            Err(e) if e == "quit" => {
                app.teardown().ok();
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    };

    match backend.get_tree(&current_tree_id) {
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

    if let Err(e) = replay_entries(backend, &current_tree_id, &mut state, &mut persistent) {
        state.push_history(HistoryItem::Notification {
            level: NotificationLevel::Warning,
            message: format!("Failed to load history: {}", e),
        });
        state.scroll_to_bottom();
    }
    app.draw(&mut state).map_err(|e| e.to_string())?;

    // ── Unified event loop ────────────────────────────────────────────────
    //
    // Each iteration:
    //   1. wait_for_event: blocks on WS fds + stdin until data arrives or
    //      the next spinner tick is due (idle: 16 ms so input stays snappy).
    //   2. Drain all pending WS events.
    //   3. Drain all pending input events.
    //   4. Advance spinner (time-based).
    //   5. Draw once.

    let mut streaming: Option<StreamingState> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            if let Some(ref mut s) = streaming {
                if !s.cancel_signalled {
                    let _ = s.session.send_stop();
                }
            } else {
                state.push_history(HistoryItem::Notification {
                    level: NotificationLevel::Warning,
                    message: "Interrupted".into(),
                });
                state.scroll_to_bottom();
                app.draw(&mut state).ok();
                break;
            }
        }

        // 1. Wait: block on WS fds + stdin together.
        let ws_fds: &[RawFd] = streaming.as_ref().map(|s| s.ws_fds.as_slice()).unwrap_or(&[]);
        let timeout = if streaming.is_some() {
            let elapsed = streaming.as_ref().unwrap().last_spinner.elapsed();
            SPINNER_INTERVAL.saturating_sub(elapsed).max(Duration::from_millis(1))
        } else {
            Duration::from_millis(16)
        };
        wait_for_event(ws_fds, timeout);

        // 2. Drain WS events.
        let mut became_done = false;
        if let Some(ref mut s) = streaming {
            let width = app.width();
            loop {
                match s.session.try_next_event() {
                    TryEvent::Event(ev) => {
                        let skip = matches!(&ev, ServerEvent::Entry(_))
                            || matches!(&ev, ServerEvent::Done { status, .. } if status == "history");
                        if skip { continue; }

                        let is_done = matches!(&ev, ServerEvent::Done { .. });
                        let is_fatal = matches!(&ev, ServerEvent::Notification {
                            level: NotificationLevel::Fatal, ..
                        });
                        apply_event(&ev, &mut state, s, &mut persistent, width);
                        if is_done || is_fatal {
                            became_done = true;
                            break;
                        }
                    }
                    TryEvent::Closed => {
                        // Connection closed without Done — finalize whatever we have.
                        let width = app.width();
                        if state.active.is_some() {
                            let active = state.active.as_mut().unwrap();
                            let _ = s.md.flush(&mut |spans| { active.push_content_spans(spans); Ok(()) }, width as usize);
                            state.finalize_active();
                        }
                        became_done = true;
                        break;
                    }
                    TryEvent::Err(e) => {
                        state.push_history(HistoryItem::Notification {
                            level: NotificationLevel::Error,
                            message: format!("ws error: {}", e),
                        });
                        state.scroll_to_bottom();
                        became_done = true;
                        break;
                    }
                    TryEvent::WouldBlock => break,
                }
            }
        }
        if became_done {
            streaming = None;
            state.spinner_active = false;
            build_status(&mut state, &persistent);
        }

        // 3. Drain all pending input events.
        loop {
            match app.poll_event(&mut state, Duration::ZERO).map_err(|e| e.to_string())? {
                Some(AppEvent::Submit(text)) if streaming.is_none() => {
                    if !text.is_empty() && input_history.last().map(|s| s.as_str()) != Some(&text) {
                        input_history.push(text.clone());
                    }
                    history_idx = None;

                    state.push_history(HistoryItem::User(text.clone()));
                    state.scroll_to_bottom();

                    match StreamingState::start(backend, &current_tree_id, &text) {
                        Ok(s) => {
                            state.spinner_active = true;
                            build_status(&mut state, &persistent);
                            streaming = Some(s);
                        }
                        Err(e) => {
                            state.push_history(HistoryItem::Notification {
                                level: NotificationLevel::Error,
                                message: e,
                            });
                            state.scroll_to_bottom();
                        }
                    }
                }

                Some(AppEvent::Cancel) => {
                    if let Some(ref mut s) = streaming {
                        if !s.cancel_signalled {
                            s.cancel_signalled = true;
                            state.push_history(HistoryItem::Notification {
                                level: NotificationLevel::Info,
                                message: "⏸ Cancelling…".into(),
                            });
                            state.scroll_to_bottom();
                            let _ = s.session.send_stop();
                        }
                    } else {
                        // Ctrl-C while idle → quit
                        app.teardown().ok();
                        return Ok(());
                    }
                }

                Some(AppEvent::HistoryPrev) if streaming.is_none() => {
                    if !input_history.is_empty() {
                        match history_idx {
                            None => {
                                history_draft = app.textarea.lines().join("\n");
                                history_idx = Some(input_history.len() - 1);
                            }
                            Some(0) => {}
                            Some(ref mut i) => *i = i.saturating_sub(1),
                        }
                        if let Some(i) = history_idx {
                            app.textarea = tui_textarea::TextArea::from(
                                input_history[i].lines().map(String::from).collect::<Vec<_>>()
                            );
                            app.textarea.set_cursor_line_style(ratatui::style::Style::default());
                        }
                    }
                }

                Some(AppEvent::HistoryNext) if streaming.is_none() => {
                    match history_idx {
                        None => {}
                        Some(i) if i + 1 >= input_history.len() => {
                            history_idx = None;
                            app.textarea = tui_textarea::TextArea::from(
                                history_draft.lines().map(String::from).collect::<Vec<_>>()
                            );
                            app.textarea.set_cursor_line_style(ratatui::style::Style::default());
                        }
                        Some(ref mut i) => {
                            *i += 1;
                            let idx = *i;
                            app.textarea = tui_textarea::TextArea::from(
                                input_history[idx].lines().map(String::from).collect::<Vec<_>>()
                            );
                            app.textarea.set_cursor_line_style(ratatui::style::Style::default());
                        }
                    }
                }

                Some(ev) => { handle_nav_event(ev, &mut state, &mut app)?; }

                None => break,
            }
        }

        // 4. Advance spinner (time-based).
        if let Some(ref mut s) = streaming {
            if s.last_spinner.elapsed() >= SPINNER_INTERVAL {
                state.spinner_frame = (state.spinner_frame + 1) % 10;
                s.last_spinner = Instant::now();
            }
        }

        // 5. Draw.
        app.draw(&mut state).map_err(|e| e.to_string())?;
    }

    app.teardown().ok();
    Ok(())
}

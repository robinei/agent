//! Interactive TUI loop for the agent CLI.
//!
//! Two-thread architecture:
//! - **SSE thread:** reads events from the server's SSE stream, pushes to an mpsc queue.
//! - **Main thread:** renders events from the queue events and polls stdin for user input.
//!
//! The main input loop and tree-selection prompts all use Terminal (crossterm)
//! for raw terminal I/O.

use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::style::{Attribute, Color, ContentStyle};

use agent_core::types::{
    ContextStatus, DiagnosticSeverity, Entry, NotificationLevel, ServerEvent, TreeMeta,
    lang_display,
};

use crate::client::TryEvent;
use crate::markdown::MarkdownEmitter;
use crate::terminal::{Span, TermEvent, Terminal};
use crate::Backend;

// ── Command parsing ──

enum CliCommand {
    Message(String),
    ListTrees,
    Create {
        title: String,
        repo_path: Option<String>,
        model: Option<String>,
    },
    Switch(String),
    Stop,
    Show,
    Help,
    Quit,
}

fn parse_input(line: &str) -> CliCommand {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return CliCommand::Message(trimmed.to_string());
    }

    let parts: Vec<&str> = trimmed.splitn(4, ' ').collect();
    match parts[0] {
        "/trees" | "/ls" => CliCommand::ListTrees,
        "/create" => {
            let title = parts.get(1).unwrap_or(&"").to_string();
            let repo_path = parts
                .get(2)
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            let model = parts
                .get(3)
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            CliCommand::Create {
                title,
                repo_path,
                model,
            }
        }
        "/switch" => {
            let id = parts.get(1).unwrap_or(&"").to_string();
            CliCommand::Switch(id)
        }
        "/stop" => CliCommand::Stop,
        "/show" => CliCommand::Show,
        "/help" => CliCommand::Help,
        "/quit" | "/exit" => CliCommand::Quit,
        _ => CliCommand::Help,
    }
}

// ── Rendering helpers (Terminal-aware) ──

fn print_warning(term: &mut Terminal, text: &str) {
    let style = ContentStyle {
        foreground_color: Some(Color::Yellow),
        attributes: Attribute::Bold.into(),
        ..Default::default()
    };
    let _ = term.append(&[Span::styled(format!("⚠ {}\r\n", text), style)]);
    let _ = term.flush_append();
}

fn print_error(term: &mut Terminal, text: &str) {
    let style = ContentStyle {
        foreground_color: Some(Color::Red),
        attributes: Attribute::Bold.into(),
        ..Default::default()
    };
    let _ = term.append(&[Span::styled(format!("✖ {}\r\n", text), style)]);
    let _ = term.flush_append();
}

fn print_indented(term: &mut Terminal, text: &str, prefix: &str) {
    let dim = ContentStyle {
        foreground_color: Some(Color::DarkGrey),
        ..Default::default()
    };
    for line in text.lines() {
        let _ = term.append(&[Span::styled(format!("  {} {}\r\n", prefix, line), dim)]);
    }
    let _ = term.flush_append();
}

fn print_help(term: &mut Terminal) {
    let bold = ContentStyle {
        attributes: Attribute::Bold.into(),
        ..Default::default()
    };
    let _ = term.append(&[Span::styled("Commands:", bold), Span::plain("\r\n")]);
    let _ = term.append(&[Span::plain(
        "  /trees                      List all trees\r\n",
    )]);
    let _ = term.append(&[Span::plain(
        "  /create <title> [path] [model]  Create a new tree\r\n",
    )]);
    let _ = term.append(&[Span::plain(
        "  /switch <id>                Switch to a different tree\r\n",
    )]);
    let _ = term.append(&[Span::plain(
        "  /stop                       Stop the active agent\r\n",
    )]);
    let _ = term.append(&[Span::plain(
        "  /show                       Show current tree info\r\n",
    )]);
    let _ = term.append(&[Span::plain("  /help                       Show this help\r\n")]);
    let _ = term.append(&[Span::plain("  /quit                       Exit\r\n")]);
    let _ = term.append(&[Span::plain(
        "  <any text>                  Send as message to the agent\r\n",
    )]);
    let _ = term.flush_append();
}

fn print_tree_meta(term: &mut Terminal, meta: &TreeMeta, index: usize) {
    let status = if meta.leaf_id.is_some() {
        "active"
    } else {
        "empty"
    };
    let title = meta.title.as_deref().unwrap_or("untitled");
    let short_id = if meta.id.len() > 8 {
        &meta.id[..8]
    } else {
        &meta.id
    };
    let _ = term.append(&[Span::plain(format!(
        "  [{}] {} — {} ({})\r\n",
        index + 1,
        short_id,
        title,
        status
    ))]);
    let _ = term.flush_append();
}

// ── Tool arg formatting ──

fn format_tool_args(tool: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let pick = match tool {
        "bash" => obj.get("command"),
        "read" => {
            let path = obj
                .get("file_path")
                .or_else(|| obj.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
            let id = obj
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string());
            let mode = obj
                .get("mode")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let s = match (id, mode) {
                (Some(id), Some(mode)) => format!("{id}  {mode}"),
                (Some(id), None) => id,
                (None, Some(mode)) => mode,
                (None, None) => String::new(),
            };
            return s;
        }
        _ => None,
    };
    match pick.and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let raw = serde_json::to_string(input).unwrap_or_default();
            if raw.len() > 80 {
                format!("{}…", &raw[..80])
            } else {
                raw
            }
        }
    }
}

// ── RenderState and render_event ──

#[derive(Default)]
struct RenderState {
    trailing_newlines: u8,
    in_thinking: bool,
    assistant_header_shown: bool,
    last_tool_args: Option<(String, serde_json::Value)>,
    model: Option<String>,
    input_rate: f64,
    output_rate: f64,
    context_pct: Option<u8>,
    context_estimated: u64,
    cum_prompt_tokens: u64,
    cum_completion_tokens: u64,
    cum_cached_tokens: u64,
    cache_supported: bool,
    last_turn_cache_pct: Option<u8>,
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

fn render_status_bar(term: &mut Terminal, state: &RenderState) {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("🤖 {}", state.model.as_deref().unwrap_or("—")));
    let ctx_pct = state.context_pct.unwrap_or(0);
    parts.push(format!("📊 {}k ({}%)", state.context_estimated / 1000, ctx_pct));
    if state.model.is_some() {
        let cost = state.cum_prompt_tokens as f64 * state.input_rate
            + state.cum_completion_tokens as f64 * state.output_rate;
        parts.push(format!("💰 ${:.4}", cost));
    } else {
        parts.push("💰 ?".into());
    }
    let session_rate = if state.cum_prompt_tokens > 0 {
        state.cum_cached_tokens as f64 / state.cum_prompt_tokens as f64 * 100.0
    } else {
        0.0
    };
    match (state.cache_supported, state.last_turn_cache_pct) {
        (true, Some(last)) => parts.push(format!("💾 {:.0}% (last {:.0}%)", session_rate, last)),
        (true, None) => parts.push(format!("💾 {:.0}%", session_rate)),
        (false, _) => parts.push("💾 ?".into()),
    }
    let text = parts.join("  ");
    let style = ContentStyle {
        foreground_color: Some(Color::White),
        background_color: Some(Color::DarkGrey),
        ..Default::default()
    };
    let _ = term.set_status(&[Span::styled(text, style)]);
}

fn render_event(
    event: &ServerEvent,
    state: &mut RenderState,
    md: &mut MarkdownEmitter,
    term: &mut Terminal,
    persistent: &mut RenderState,
) -> io::Result<()> {
    let bold_style = ContentStyle {
        attributes: Attribute::Bold.into(),
        ..Default::default()
    };
    let dim_style = ContentStyle {
        foreground_color: Some(Color::DarkGrey),
        ..Default::default()
    };
    let green = ContentStyle {
        foreground_color: Some(Color::Green),
        ..Default::default()
    };
    let yellow = ContentStyle {
        foreground_color: Some(Color::Yellow),
        ..Default::default()
    };
    let red = ContentStyle {
        foreground_color: Some(Color::Red),
        ..Default::default()
    };
    let light_black = ContentStyle {
        foreground_color: Some(Color::DarkGrey),
        ..Default::default()
    };
    let grey_red = ContentStyle {
        foreground_color: Some(Color::Rgb { r: 190, g: 90, b: 90 }),
        ..Default::default()
    };

    match event {
        ServerEvent::TextChunk { content } => {
            if state.in_thinking {
                state.in_thinking = false;
                term.append(&[Span::styled("\r\n", bold_style)])?;
                state.assistant_header_shown = true;
            }
            if !state.assistant_header_shown {
                state.assistant_header_shown = true;
                term.append(&[Span::plain("\r\n")])?;
                state.trailing_newlines = 1;
            }
            let tw = term.cols() as usize;
            md.push(content, &mut |spans| term.append(spans), tw)?;
        }
        ServerEvent::ThinkingChunk { content } => {
            if !state.in_thinking {
                state.in_thinking = true;
                term.append(&[Span::styled("\r\n", dim_style)])?;
                state.trailing_newlines = 0;
            }
            term.append(&[Span::styled(content.clone(), dim_style)])?;
        }
        ServerEvent::ToolStart { tool, input } => {
            state.last_tool_args = Some((tool.clone(), input.clone()));
        }
        ServerEvent::ToolResult {
            tool,
            exit,
            output,
        } => {
            if state.in_thinking {
                state.in_thinking = false;
                term.append(&[Span::plain("\r\n")])?;
            }
            let args_str = state
                .last_tool_args
                .take()
                .map(|(_, input)| format_tool_args(tool, &input))
                .unwrap_or_default();
            blank_line_sep(state, term)?;
            term.append(&[
                Span::styled(format!("  ⚙ {}", tool), bold_style),
                Span::plain("  "),
            ])?;
            if !args_str.is_empty() {
                let args_display = args_str.replace('\n', "\r\n");
                term.append(&[Span::plain(args_display)])?;
            }
            let exit_style = if *exit == 0 { light_black } else { red };
            term.append(&[Span::styled(
                format!("  (exit: {})\r\n", exit),
                exit_style,
            )])?;
            if !output.is_empty() {
                print_indented(term, output, "│");
            }
            state.trailing_newlines = 1;
            term.flush_append()?;
        }
        ServerEvent::Entry(entry) => {
            match entry {
                Entry::Message { message, .. }
                    if message.role == agent_core::types::MessageRole::User =>
                {
                    let t = match &message.content {
                        agent_core::types::MessageContent::Text(t) => t.clone(),
                        _ => "[content blocks]".into(),
                    };
                    term.append(&[
                        Span::styled("> ", green),
                        Span::plain(t),
                        Span::plain("\n"),
                    ])?;
                    state.trailing_newlines = 0;
                }
                Entry::GoalSet { goal, .. } => {
                    term.append(&[Span::plain(format!("🎯  {}\r\n", goal))])?;
                    state.trailing_newlines = 0;
                }
                Entry::ModelSet { model, .. } => {
                    term.append(&[Span::plain(format!("🤖  Model: {}\r\n", model))])?;
                    state.trailing_newlines = 0;
                }
                Entry::SessionEnd {
                    summary, status, ..
                } => {
                    let s = summary.as_deref().unwrap_or("");
                    let msg = if s.is_empty() {
                        format!("📝 Session ended ({:?})\r\n", status)
                    } else {
                        format!("📝 Session ended ({:?}): {}\r\n", status, s)
                    };
                    term.append(&[Span::styled(msg, bold_style)])?;
                    state.trailing_newlines = 0;
                }
                Entry::Message { message, .. } => {
                    if let Some(ref thinking) = message.thinking {
                        if !thinking.is_empty() {
                            if !state.in_thinking {
                                state.in_thinking = true;
                                term.append(&[Span::styled("\r\n", dim_style)])?;
                                state.trailing_newlines = 0;
                            }
                            term.append(&[Span::styled(thinking.clone(), dim_style)])?;
                        }
                    }
                    let t = match &message.content {
                        agent_core::types::MessageContent::Text(t) => t.clone(),
                        _ => "[content blocks]".into(),
                    };
                    if !t.is_empty() {
                        if state.in_thinking {
                            state.in_thinking = false;
                            term.append(&[Span::styled("\r\n", bold_style)])?;
                            state.assistant_header_shown = true;
                        }
                        let tw = term.cols() as usize;
                        md.push(&t, &mut |spans| term.append(spans), tw)?;
                        md.flush(&mut |spans| term.append(spans), tw)?;
                        state.trailing_newlines = 0;
                    }
                }
                Entry::BashExec {
                    command,
                    output,
                    exit_code,
                    ..
                } => {
                    term.append(&[
                        Span::styled(format!("  🛠  bash: {}", command), yellow),
                        Span::plain("\r\n"),
                    ])?;
                    let c = if *exit_code == 0 { green } else { red };
                    term.append(&[Span::styled(
                        format!("  bash (exit: {})\r\n", exit_code),
                        c,
                    )])?;
                    if !output.is_empty() {
                        print_indented(term, output, "│");
                    }
                    state.trailing_newlines = 0;
                }
                _ => {}
            }
            term.flush_append()?;
        }
        ServerEvent::ContextUpdate { status, pct, estimated } => {
            match status {
                ContextStatus::Warning | ContextStatus::Critical => {
                    print_warning(term, &format!("Context at {}% ({:?})", pct, status));
                    state.trailing_newlines = 1;
                }
                _ => {}
            }
            let changed = state.context_pct != Some(*pct) || state.context_estimated != *estimated;
            state.context_pct = Some(*pct);
            state.context_estimated = *estimated;
            if changed {
                render_status_bar(term, state);
            }
        }
        ServerEvent::Notification { level, message } => {
            if state.in_thinking {
                state.in_thinking = false;
                term.append(&[Span::plain("\r\n")])?;
            }
            match level {
                NotificationLevel::Info => {
                    term.append(&[Span::styled(
                        format!("  {}\r\n", message),
                        yellow,
                    )])?;
                }
                NotificationLevel::Warning => {
                    term.append(&[Span::styled(
                        format!("  {}\r\n", message),
                        grey_red,
                    )])?;
                }
                NotificationLevel::Error => {
                    let style = ContentStyle {
                        foreground_color: Some(Color::Red),
                        attributes: Attribute::Bold.into(),
                        ..Default::default()
                    };
                    term.append(&[Span::styled(
                        format!("  Error: {}\r\n", message),
                        style,
                    )])?;
                }
                NotificationLevel::Fatal => {
                    let style = ContentStyle {
                        foreground_color: Some(Color::Red),
                        attributes: Attribute::Bold.into(),
                        ..Default::default()
                    };
                    term.append(&[Span::styled(
                        format!("  Fatal: {}\r\n", message),
                        style,
                    )])?;
                }
            }
            state.trailing_newlines = 0;
            term.flush_append()?;
        }
        ServerEvent::Diagnostics { source, files } => {
            if state.in_thinking {
                state.in_thinking = false;
                term.append(&[Span::plain("\r\n")])?;
            }

            let new_errors: usize = files
                .iter()
                .flat_map(|f| &f.diagnostics)
                .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::Error)))
                .count();
            let new_warnings: usize = files
                .iter()
                .flat_map(|f| &f.diagnostics)
                .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::Warning)))
                .count();

            let header_color = if new_errors > 0 {
                Color::Red
            } else if new_warnings > 0 {
                Color::Yellow
            } else {
                Color::DarkGrey
            };

            blank_line_sep(state, term)?;
            term.append(&[Span::styled(
                format!("  ◈ {}\r\n", lang_display(source)),
                ContentStyle {
                    foreground_color: Some(header_color),
                    ..Default::default()
                },
            )])?;

            for file in files {
                let display_path = std::path::Path::new(&file.path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file.path);
                term.append(&[Span::styled(
                    format!("    {}\r\n", display_path),
                    bold_style,
                )])?;

                let line_width = file
                    .diagnostics
                    .iter()
                    .map(|d| (d.range.start.line + 1).to_string().len())
                    .max()
                    .unwrap_or(1);
                for diag in &file.diagnostics {
                    let (col_style, label) = sev_color_label(diag.severity);
                    let first_line = diag.message.lines().next().unwrap_or("");
                    let msg: String = if first_line.chars().count() > 72 {
                        format!(
                            "{}…",
                            first_line.chars().take(71).collect::<String>()
                        )
                    } else {
                        first_line.to_string()
                    };
                    term.append(&[Span::styled(
                        format!("      {} ", label),
                        col_style,
                    )])?;
                    term.append(&[Span::styled(
                        format!(
                            "{:>width$}  {}\r\n",
                            diag.range.start.line + 1,
                            msg,
                            width = line_width
                        ),
                        dim_style,
                    )])?;
                }

                let summary = seen_summary(file.seen_errors, file.seen_warnings);
                if !summary.is_empty() {
                    term.append(&[Span::styled(
                        format!("      ({})\r\n", summary),
                        dim_style,
                    )])?;
                }
            }
            state.trailing_newlines = 1;
            term.flush_append()?;
        }
        ServerEvent::Done { status, usage } => {
            if let Some(u) = usage {
                state.cum_prompt_tokens += u.prompt_tokens;
                state.cum_completion_tokens += u.completion_tokens;
                let turn_cached = u.cached_prompt_tokens.unwrap_or(0);
                if u.prompt_tokens > 0 {
                    state.last_turn_cache_pct =
                        Some((turn_cached as f64 / u.prompt_tokens as f64 * 100.0).round() as u8);
                }
                if let Some(cached) = u.cached_prompt_tokens {
                    state.cum_cached_tokens += cached;
                    state.cache_supported = true;
                } else if state.cache_supported && state.cum_prompt_tokens > 0 {
                    let prev = state.cum_prompt_tokens - u.prompt_tokens;
                    let rate = if prev > 0 { state.cum_cached_tokens as f64 / prev as f64 } else { 0.0 };
                    state.cum_cached_tokens += (u.prompt_tokens as f64 * rate).round() as u64;
                }
                persistent.cum_prompt_tokens = state.cum_prompt_tokens;
                persistent.cum_completion_tokens = state.cum_completion_tokens;
                persistent.cum_cached_tokens = state.cum_cached_tokens;
                persistent.cache_supported = state.cache_supported;
                persistent.last_turn_cache_pct = state.last_turn_cache_pct;
                if let Some(ref m) = state.model {
                    persistent.model = Some(m.clone());
                    persistent.input_rate = state.input_rate;
                    persistent.output_rate = state.output_rate;
                }
                render_status_bar(term, state);
            }
            let tw = term.cols() as usize;
            md.flush(&mut |spans| term.append(spans), tw)?;
            if state.in_thinking {
                state.in_thinking = false;
                term.append(&[Span::plain("\r\n")])?;
            }
            if state.trailing_newlines == 0 {
                term.append(&[Span::plain("\r\n")])?;
            }
            match status.as_str() {
                "stop" | "complete" | "error" | "history" => {}
                "length" => {
                    term.append(&[Span::styled(
                        "  ⚠ Stopped at length limit\r\n",
                        yellow,
                    )])?;
                }
                "aborted" => {
                    term.append(&[Span::styled("  ✖ Aborted\r\n", red)])?;
                }
                "cancelled" => {
                    term.append(&[Span::styled("  ✋ Cancelled\r\n", yellow)])?;
                }
                other => {
                    term.append(&[Span::plain(format!(
                        "  ⚠ unknown completion status: {}\r\n",
                        other
                    ))])?;
                }
            }
            term.flush_append()?;
        }
        ServerEvent::FileChanged { path, kind } => {
            term.append(&[Span::plain(format!(
                "\r\n  📄 {} ({})\r\n",
                path, kind
            ))])?;
            state.trailing_newlines = 0;
            term.flush_append()?;
        }
        ServerEvent::MetaUpdate { title, model } => {
            if let Some(t) = title {
                term.append(&[Span::styled(
                    format!("\r\n  Title: {}\r\n", t),
                    bold_style,
                )])?;
                state.trailing_newlines = 0;
                term.flush_append()?;
            }
            if let Some(m) = model {
                state.model = Some(m.clone());
                state.input_rate = model_pricing(&m).0;
                state.output_rate = model_pricing(&m).1;
                persistent.model = Some(m.clone());
                persistent.input_rate = state.input_rate;
                persistent.output_rate = state.output_rate;
                render_status_bar(term, state);
            }
        }
    }
    Ok(())
}

fn sev_color_label(sev: Option<DiagnosticSeverity>) -> (ContentStyle, &'static str) {
    match sev {
        Some(DiagnosticSeverity::Error) => (
            ContentStyle {
                foreground_color: Some(Color::Red),
                ..Default::default()
            },
            "error  ",
        ),
        Some(DiagnosticSeverity::Warning) => (
            ContentStyle {
                foreground_color: Some(Color::Yellow),
                ..Default::default()
            },
            "warning",
        ),
        Some(DiagnosticSeverity::Information) => (
            ContentStyle {
                foreground_color: Some(Color::Cyan),
                ..Default::default()
            },
            "info   ",
        ),
        _ => (
            ContentStyle {
                foreground_color: Some(Color::DarkGrey),
                ..Default::default()
            },
            "hint   ",
        ),
    }
}

fn seen_summary(errors: u32, warnings: u32) -> String {
    match (errors, warnings) {
        (0, 0) => String::new(),
        (e, 0) => format!(
            "{} seen error{}",
            e,
            if e == 1 { "" } else { "s" }
        ),
        (0, w) => format!(
            "{} seen warning{}",
            w,
            if w == 1 { "" } else { "s" }
        ),
        (e, w) => format!(
            "{} seen error{}, {} seen warning{}",
            e,
            if e == 1 { "" } else { "s" },
            w,
            if w == 1 { "" } else { "s" }
        ),
    }
}

fn blank_line_sep(state: &mut RenderState, term: &mut Terminal) -> io::Result<()> {
    let needed = 2u8.saturating_sub(state.trailing_newlines);
    if needed > 0 {
        term.append(&[Span::plain("\r\n".repeat(needed as usize))])?;
    }
    state.trailing_newlines = 2;
    Ok(())
}

// ── wait_for_wakeup ──

#[cfg(unix)]
fn wait_for_wakeup(
    ws_fds: &[RawFd],
    term: &mut Terminal,
    timeout: Duration,
) -> io::Result<Option<TermEvent>> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::unix::io::BorrowedFd;
    let stdin_fd = {
        use std::os::unix::io::AsRawFd;
        std::io::stdin().as_raw_fd()
    };
    let pt = PollTimeout::from(timeout.as_millis().min(u16::MAX as u128) as u16);
    let mut fds: Vec<PollFd> = ws_fds
        .iter()
        .map(|&fd| unsafe { PollFd::new(BorrowedFd::borrow_raw(fd), PollFlags::POLLIN) })
        .chain(std::iter::once(unsafe {
            PollFd::new(BorrowedFd::borrow_raw(stdin_fd), PollFlags::POLLIN)
        }))
        .collect();
    let _ = poll(&mut fds, pt);
    if fds
        .last()
        .and_then(|f| f.revents())
        .is_some_and(|r| r.contains(PollFlags::POLLIN))
    {
        term.poll(Duration::ZERO)
    } else {
        Ok(None)
    }
}

#[cfg(not(unix))]
fn wait_for_wakeup(
    _ws_fds: &[RawFd],
    term: &mut Terminal,
    timeout: Duration,
) -> io::Result<Option<TermEvent>> {
    term.poll(timeout)
}

// ── Message processing ──

fn process_message(
    backend: &Backend,
    tree_id: &str,
    text: &str,
    term: &mut Terminal,
    md: &mut MarkdownEmitter,
    stop: &AtomicBool,
    persistent: &mut RenderState,
) -> Result<(), String> {
    let mut session = backend.connect_session(tree_id).map_err(|e| e.to_string())?;
    session.set_nonblocking(true)?;
    session.send_message(text).map_err(|e| e.to_string())?;
    let ws_fds: Vec<RawFd> = session.as_raw_fd().into_iter().collect();

    let mut state = RenderState {
        model: persistent.model.clone(),
        input_rate: persistent.input_rate,
        output_rate: persistent.output_rate,
        cum_prompt_tokens: persistent.cum_prompt_tokens,
        cum_completion_tokens: persistent.cum_completion_tokens,
        cum_cached_tokens: persistent.cum_cached_tokens,
        cache_supported: persistent.cache_supported,
        last_turn_cache_pct: persistent.last_turn_cache_pct,
        ..Default::default()
    };

    render_status_bar(term, &state);
    term.set_spinner_active(true).map_err(|e| e.to_string())?;

    let mut cancel_signalled = false;

    loop {
        // 1. Drain WS events
        loop {
            match session.try_next_event() {
                TryEvent::Event(ev) => {
                    render_event(&ev, &mut state, md, term, persistent)
                        .map_err(|e| e.to_string())?;
                    let is_real_done = matches!(&ev, ServerEvent::Done { status, .. } if status != "history");
                    let fatal = matches!(
                        &ev,
                        ServerEvent::Notification {
                            level: NotificationLevel::Fatal,
                            ..
                        }
                    );
                    if fatal {
                        let tw = term.cols() as usize;
                        md.flush(&mut |spans| term.append(spans), tw).map_err(|e| e.to_string())?;
                    }
                    if is_real_done || fatal {
                        term.set_spinner_active(false)
                            .map_err(|e| e.to_string())?;
                        return Ok(());
                    }
                    continue;
                }
                TryEvent::Closed => {
                    term.set_spinner_active(false)
                        .map_err(|e| e.to_string())?;
                    let tw = term.cols() as usize;
                    md.flush(&mut |spans| term.append(spans), tw).map_err(|e| e.to_string())?;
                    return Ok(());
                }
                TryEvent::Err(e) => {
                    term.set_spinner_active(false)
                        .map_err(|e| e.to_string())?;
                    let tw = term.cols() as usize;
                    md.flush(&mut |spans| term.append(spans), tw).map_err(|e| e.to_string())?;
                    let _ = term.append(&[Span::plain(format!("\r\nws error: {}\r\n", e))]);
                    let _ = term.flush_append();
                    return Ok(());
                }
                TryEvent::WouldBlock => break,
            }
        }

        // 2. Check Ctrl-C from the outer signal handler
        if stop.load(Ordering::Relaxed) {
            term.set_spinner_active(false)
                .map_err(|e| e.to_string())?;
            let tw = term.cols() as usize;
            let _ = md.flush(&mut |spans| term.append(spans), tw);
            let _ = term.append(&[Span::plain("\r\nInterrupted\r\n")]);
            let _ = term.flush_append();
            break;
        }

        // 3. Wait for more data (wake at least once per spinner tick),
        //    and check for cancel via stdin.
        let wakeup_ev = wait_for_wakeup(&ws_fds, term, crate::terminal::SPINNER_INTERVAL)
            .map_err(|e| e.to_string())?;
        // Refresh the display to advance the spinner even when no data arrives.
        term.refresh().map_err(|e| e.to_string())?;
        if !cancel_signalled {
            if matches!(wakeup_ev, Some(TermEvent::Cancel)) {
                term.set_spinner_active(false)
                    .map_err(|e| e.to_string())?;
                let _ = term.append(&[Span::styled(
                    "\r\n  ⏸ Cancelling…\r\n",
                    ContentStyle {
                        foreground_color: Some(Color::Yellow),
                        ..Default::default()
                    },
                )]);
                let _ = term.flush_append();
                let _ = session.send_stop();
                cancel_signalled = true;
                term.set_spinner_active(true)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

// ── Tree selection ──

fn select_or_create_tree(
    term: &mut Terminal,
    backend: &Backend,
) -> Result<String, String> {
    loop {
        let trees = backend.list_trees().map_err(|e| e.to_string())?;

        if !trees.is_empty() {
            let _ = term.append(&[Span::plain("\r\nYour trees:\r\n")]);
            let _ = term.flush_append();
            for (i, tree) in trees.iter().enumerate() {
                print_tree_meta(term, tree, i);
            }
            let _ = term.append(&[Span::plain("\r\n")]);
            let _ = term.append(&[Span::plain(
                "Select a tree (number), 'new', or 'q' to quit: ",
            )]);
            let _ = term.flush_append();

            let result = loop {
                match term.poll(Duration::from_millis(16)) {
                    Ok(Some(TermEvent::Submit(text))) => break text,
                    Ok(Some(TermEvent::Cancel)) => std::process::exit(0),
                    _ => {}
                }
            };
            let input = result.trim().to_lowercase();

            if input == "q" || input == "quit" {
                std::process::exit(0);
            }
            if input == "new" {
                return create_tree_interactive(term, backend);
            }

            if let Ok(idx) = input.parse::<usize>() {
                if idx > 0 && idx <= trees.len() {
                    return Ok(trees[idx - 1].id.clone());
                }
            }

            if !input.is_empty() {
                let matches: Vec<&TreeMeta> = trees
                    .iter()
                    .filter(|t| t.id.starts_with(&input))
                    .collect();
                if matches.len() == 1 {
                    return Ok(matches[0].id.clone());
                }
                if matches.len() > 1 {
                    let _ = term.append(&[Span::plain(
                        "Multiple matches, be more specific.\r\n",
                    )]);
                    let _ = term.flush_append();
                    continue;
                }
            }

            let _ = term.append(&[Span::plain("Invalid selection.\r\n")]);
            let _ = term.flush_append();
        } else {
            let _ = term.append(&[Span::plain(
                "No trees found. Let's create one.\r\n",
            )]);
            let _ = term.flush_append();
            return create_tree_interactive(term, backend);
        }
    }
}

fn create_tree_interactive(
    term: &mut Terminal,
    backend: &Backend,
) -> Result<String, String> {
    let green = ContentStyle {
        foreground_color: Some(Color::Green),
        ..Default::default()
    };

    let mut input_text = |prompt: &str| -> String {
        let _ = term.append(&[Span::plain(prompt)]);
        let _ = term.flush_append();
        loop {
            match term.poll(Duration::from_millis(16)) {
                Ok(Some(TermEvent::Submit(text))) => return text,
                Ok(Some(TermEvent::Cancel)) => return String::new(),
                _ => {}
            }
        }
    };

    let title = input_text("Enter a title (or press Enter for 'default'): ");
    let title = title.trim().to_string();
    let title = if title.is_empty() {
        "default".into()
    } else {
        title
    };

    let repo_path = input_text("Enter repo path (optional): ");
    let repo_path = repo_path.trim().to_string();
    let repo_path = if repo_path.is_empty() {
        None
    } else {
        Some(repo_path)
    };

    let model = input_text("Enter model (optional): ");
    let model = model.trim().to_string();
    let model = if model.is_empty() { None } else { Some(model) };

    let meta = backend.create_tree(
        Some(&title),
        repo_path.as_deref(),
        model.as_deref(),
        &[],
        None,
        &[],
        &[],
    )
    .map_err(|e| e.to_string())?;
    let short_id = if meta.id.len() > 8 {
        &meta.id[..8]
    } else {
        &meta.id
    };
    let _ = term.append(&[Span::styled(
        format!(
            "Created tree {} ({})\r\n",
            short_id,
            meta.title.as_deref().unwrap_or("untitled")
        ),
        green,
    )]);
    let _ = term.flush_append();
    Ok(meta.id)
}

// ── History replay ──

fn replay_entries(
    backend: &Backend,
    tree_id: &str,
    term: &mut Terminal,
    md: &mut MarkdownEmitter,
) -> Result<(), String> {
    let mut session = backend.connect_session(tree_id).map_err(|e| e.to_string())?;
    session.set_nonblocking(true).map_err(|e| e.to_string())?;
    let ws_fds: Vec<RawFd> = session.as_raw_fd().into_iter().collect();

    let mut state = RenderState::default();
    let mut replay_sticky = RenderState::default();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        loop {
            match session.try_next_event() {
                TryEvent::Event(ServerEvent::Done { status, .. }) if status == "history" => {
                    return Ok(());
                }
                TryEvent::Event(ev) => {
                    render_event(&ev, &mut state, md, term, &mut replay_sticky)
                        .map_err(|e| e.to_string())?;
                }
                TryEvent::WouldBlock => break,
                TryEvent::Closed | TryEvent::Err(_) => return Ok(()),
            }
        }

        if std::time::Instant::now() >= deadline {
            break;
        }

        let timeout = std::cmp::min(
            deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as u64,
            200u64,
        );
        wait_for_wakeup(&ws_fds, term, std::time::Duration::from_millis(timeout))
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

// ── Prompt loop ──

/// Run the interactive TUI.
pub fn run_interactive(
    backend: &Backend,
    initial_repo_path: Option<String>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut term = Terminal::new("> ").map_err(|e| format!("terminal init: {}", e))?;
    let mut md = MarkdownEmitter::new();
    let mut history: Vec<String> = Vec::new();
    let mut history_idx: Option<usize> = None;
    let mut history_draft = String::new();

    let green = ContentStyle {
        foreground_color: Some(Color::Green),
        ..Default::default()
    };
    let _ = term.append(&[Span::styled("Connected", green)]);
    let _ = term.flush_append();
    print_help(&mut term);
    let _ = term.append(&[Span::plain("\r\n")]);
    let _ = term.flush_append();

    let mut current_tree_id = if let Some(rp) = initial_repo_path {
        let meta = backend
            .create_tree(Some("untitled"), Some(&rp), None, &[], None, &[], &[])
            .map_err(|e| format!("failed to create tree: {}", e))?;
        let short_id = if meta.id.len() > 8 {
            &meta.id[..8]
        } else {
            &meta.id
        };
        let _ = term.append(&[Span::styled(
            format!("Created tree {} in {}\r\n", short_id, rp),
            green,
        )]);
        let _ = term.flush_append();
        meta.id
    } else {
        select_or_create_tree(&mut term, backend)?
    };
    let mut show_header = true;
    let mut persistent_state = RenderState::default();

    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = term.append(&[Span::plain("\r\nInterrupted\r\n")]);
            let _ = term.flush_append();
            break;
        }
        if show_header {
            match backend.get_tree(&current_tree_id) {
                Ok(meta) => {
                    let title = meta.title.as_deref().unwrap_or("untitled");
                    let short_id = if current_tree_id.len() > 8 {
                        &current_tree_id[..8]
                    } else {
                        &current_tree_id
                    };
                    let bold = ContentStyle {
                        attributes: Attribute::Bold.into(),
                        ..Default::default()
                    };
                    let light_black = ContentStyle {
                        foreground_color: Some(Color::DarkGrey),
                        ..Default::default()
                    };
                    let _ = term.append(&[
                        Span::styled(title.to_string(), bold),
                        Span::styled(format!("  ·  {}", short_id), light_black),
                        Span::plain("\r\n"),
                    ]);
                    let _ = term.flush_append();
                }
                Err(e) => print_warning(&mut term, &format!("Failed to load tree: {}", e)),
            }
            show_header = false;

            if let Err(e) = replay_entries(backend, &current_tree_id, &mut term, &mut md) {
                print_warning(&mut term, &format!("Failed to load history: {}", e));
            }
        }

        // Prompt for input
        let result = loop {
            match term.poll(Duration::from_millis(16)).map_err(|e| e.to_string())? {
                Some(TermEvent::Submit(text)) => {
                    if !text.is_empty() && history.last() != Some(&text) {
                        history.push(text.clone());
                    }
                    history_idx = None;
                    break text;
                }
                Some(TermEvent::Cancel) => {
                    let _ = term.append(&[Span::plain("Goodbye!\r\n")]);
                    let _ = term.flush_append();
                    term.teardown().ok();
                    return Ok(());
                }
                Some(TermEvent::HistoryPrev) => {
                    if history.is_empty() {
                        continue;
                    }
                    match history_idx {
                        None => {
                            history_draft = term.input().to_string();
                            history_idx = Some(history.len() - 1);
                        }
                        Some(0) => {}
                        Some(ref mut i) => *i = i.saturating_sub(1),
                    }
                    if let Some(i) = history_idx {
                        let _ = term.set_input(&history[i]);
                    }
                }
                Some(TermEvent::HistoryNext) => {
                    match history_idx {
                        None => {}
                        Some(i) if i + 1 >= history.len() => {
                            history_idx = None;
                            let _ = term.set_input(&history_draft);
                        }
                        Some(ref mut i) => {
                            *i += 1;
                            let _ = term.set_input(&history[*i]);
                        }
                    }
                }
                _ => {}
            }
        };
        let input = result.trim().to_string();
        if input.is_empty() {
            continue;
        }

        match parse_input(&input) {
            CliCommand::Quit => {
                let _ = term.append(&[Span::plain("Goodbye!\r\n")]);
                let _ = term.flush_append();
                term.teardown().ok();
                break;
            }
            CliCommand::Help => print_help(&mut term),
            CliCommand::ListTrees => match backend.list_trees() {
                Ok(trees) => {
                    let _ = term.append(&[Span::plain("\r\n")]);
                    let _ = term.flush_append();
                    for (i, t) in trees.iter().enumerate() {
                        print_tree_meta(&mut term, t, i);
                    }
                }
                Err(e) => print_error(
                    &mut term,
                    &format!("Failed to list trees: {}", e),
                ),
            },
            CliCommand::Create {
                title,
                repo_path,
                model,
            } => match backend.create_tree(
                if title.is_empty() { None } else { Some(&title) },
                repo_path.as_deref(),
                model.as_deref(),
                &[],
                None,
                &[],
                &[],
            ) {
                Ok(meta) => {
                    current_tree_id = meta.id.clone();
                    show_header = true;
                    let short_id = if meta.id.len() > 8 {
                        &meta.id[..8]
                    } else {
                        &meta.id
                    };
                    let _ = term.append(&[Span::styled(
                        format!(
                            "Created tree {} ({})\r\n",
                            short_id,
                            meta.title.as_deref().unwrap_or("untitled")
                        ),
                        green,
                    )]);
                    let _ = term.flush_append();
                }
                Err(e) => print_error(
                    &mut term,
                    &format!("Failed to create tree: {}", e),
                ),
            },
            CliCommand::Switch(id) => {
                if id.is_empty() {
                    print_error(&mut term, "Usage: /switch <tree_id>");
                    continue;
                }
                match backend.get_tree(&id) {
                    Ok(meta) => {
                        current_tree_id = meta.id;
                        show_header = true;
                        let _ = term.append(&[Span::styled(
                            format!(
                                "Switched to tree {}\r\n",
                                meta.title.as_deref().unwrap_or(&id)
                            ),
                            green,
                        )]);
                        let _ = term.flush_append();
                    }
                    Err(e) => print_error(
                        &mut term,
                        &format!("Tree not found: {}", e),
                    ),
                }
            }
            CliCommand::Stop => match backend.stop_agent(&current_tree_id) {
                Ok(()) => {
                    let yellow = ContentStyle {
                        foreground_color: Some(Color::Yellow),
                        ..Default::default()
                    };
                    let _ = term.append(&[Span::styled("Stop signaled\r\n", yellow)]);
                    let _ = term.flush_append();
                }
                Err(e) => print_error(&mut term, &format!("Failed to stop: {}", e)),
            },
            CliCommand::Show => match backend.get_tree(&current_tree_id) {
                Ok(meta) => {
                    let bold = ContentStyle {
                        attributes: Attribute::Bold.into(),
                        ..Default::default()
                    };
                    let _ = term.append(&[Span::styled("Tree info:\r\n", bold)]);
                    let _ = term.append(&[Span::plain(format!(
                        "  ID:        {}\r\n",
                        meta.id
                    ))]);
                    let _ = term.append(&[Span::plain(format!(
                        "  Title:     {}\r\n",
                        meta.title.as_deref().unwrap_or("(none)")
                    ))]);
                    let _ = term.append(&[Span::plain(format!(
                        "  Repo path: {}\r\n",
                        meta.repo_path
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or("(none)".into())
                    ))]);
                    let _ = term.append(&[Span::plain(format!(
                        "  Active:    {}\r\n",
                        if meta.leaf_id.is_some() {
                            "yes"
                        } else {
                            "no"
                        }
                    ))]);
                    let _ = term.flush_append();
                }
                Err(e) => {
                    print_error(&mut term, &format!("Failed to load tree: {}", e))
                }
            },
            CliCommand::Message(text) => {
                process_message(
                    backend,
                    &current_tree_id,
                    &text,
                    &mut term,
                    &mut md,
                    stop,
                    &mut persistent_state,
                )?;
            }
        }
    }

    term.teardown().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn render_done(out: &mut impl Write, status: &str) {
        match status {
            "stop" | "complete" | "error" => {}
            "length" => {
                let _ = write!(out, "\r\n  ⚠ Stopped at length limit\r\n");
            }
            "aborted" => {
                let _ = write!(out, "\r\n  ✖ Aborted\r\n");
            }
            "cancelled" => {
                let _ = write!(out, "\r\n  ✋ Cancelled\r\n");
            }
            other => {
                let _ = write!(
                    out,
                    "\r\n  ⚠ unknown completion status: {}\r\n",
                    other
                );
            }
        }
    }

    #[test]
    fn test_render_done_stop_is_silent() {
        let mut buf = Vec::new();
        render_done(&mut buf, "stop");
        assert!(buf.is_empty(), "stop should produce no output");
    }

    #[test]
    fn test_render_done_complete_is_silent() {
        let mut buf = Vec::new();
        render_done(&mut buf, "complete");
        assert!(buf.is_empty(), "complete should produce no output");
    }

    #[test]
    fn test_render_done_aborted_is_red() {
        let mut buf = Vec::new();
        render_done(&mut buf, "aborted");
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains('✖'),
            "aborted should show ✖, got: {output}"
        );
        assert!(
            output.contains("Aborted"),
            "aborted should show Aborted, got: {output}"
        );
    }

    #[test]
    fn test_render_done_cancelled() {
        let mut buf = Vec::new();
        render_done(&mut buf, "cancelled");
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains('✋'),
            "cancelled should show ✋, got: {output}"
        );
        assert!(
            output.contains("Cancelled"),
            "cancelled should show Cancelled, got: {output}"
        );
        assert!(
            !output.contains("Done"),
            "cancelled should not show Done, got: {output}"
        );
    }

    #[test]
    fn test_render_done_length() {
        let mut buf = Vec::new();
        render_done(&mut buf, "length");
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains('⚠'),
            "length should show ⚠, got: {output}"
        );
        assert!(
            output.contains("Stopped at length limit"),
            "length should show warning, got: {output}"
        );
    }

    #[test]
    fn test_format_tool_args_bash() {
        let result = format_tool_args(
            "bash",
            &serde_json::json!({"command":"ls","description":"d"}),
        );
        assert_eq!(result, "ls");
    }

    #[test]
    fn test_format_tool_args_fallback() {
        let result = format_tool_args("unknown_tool", &serde_json::json!({"key":"value"}));
        assert_eq!(result, "{\"key\":\"value\"}");
    }
}

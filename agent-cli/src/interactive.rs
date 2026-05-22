//! Interactive TUI loop for the agent CLI.
//!
//! Two-thread architecture:
//! - **SSE thread:** reads events from the server's SSE stream, pushes to an mpsc queue.
//! - **Main thread:** renders events from the queue events and polls stdin for user input.
//!
//! Ctrl-C is handled via termion raw mode: `Key::Ctrl('c')` is detected directly from key events.
//! The main input loop and tree-selection prompts all use character-by-character raw input.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use termion::event::Key;
use termion::input::TermRead;
use termion::raw::IntoRawMode;
use termion::{clear, color, style};

use agent_core::types::{Entry, NotificationLevel, ServerEvent, TreeMeta, lang_display};

use crate::client::TryEvent;

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
    Entries(Option<usize>),
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
        "/entries" => {
            let n = parts.get(1).and_then(|s| s.parse::<usize>().ok());
            CliCommand::Entries(n)
        }
        "/help" => CliCommand::Help,
        "/quit" | "/exit" => CliCommand::Quit,
        _ => CliCommand::Help,
    }
}

// ── Rendering helpers (raw-mode aware ──

fn print_warning(out: &mut impl Write, text: &str) {
    write!(
        out,
        "{}{}⚠ {}{}\r\n",
        color::Fg(color::Yellow),
        style::Bold,
        text,
        style::Reset
    )
    .ok();
}

fn print_error(out: &mut impl Write, text: &str) {
    let text = normalize_for_raw(text);
    write!(
        out,
        "{}{}✖ {}{}\r\n",
        color::Fg(color::Red),
        style::Bold,
        text,
        style::Reset
    )
    .ok();
}

fn print_indented(out: &mut impl Write, text: &str, prefix: &str) {
    for line in text.lines() {
        write!(out, "  {} {}\r\n", prefix, line).ok();
    }
}

fn print_help(out: &mut impl Write) {
    write!(out, "{}Commands:{}", style::Bold, style::Reset).ok();
    writeln!(out).ok();
    write!(out, "\r").ok();
    write!(out, "  /trees                      List all trees\r\n").ok();
    write!(
        out,
        "  /create <title> [path] [model]  Create a new tree\r\n"
    )
    .ok();
    write!(
        out,
        "  /switch <id>                Switch to a different tree\r\n"
    )
    .ok();
    write!(
        out,
        "  /stop                       Stop the active agent\r\n"
    )
    .ok();
    write!(
        out,
        "  /show                       Show current tree info\r\n"
    )
    .ok();
    write!(
        out,
        "  /entries [n]                Show last N entries (default 10)\r\n"
    )
    .ok();
    write!(out, "  /help                       Show this help\r\n").ok();
    write!(out, "  /quit                       Exit\r\n").ok();
    write!(
        out,
        "  <any text>                  Send as message to the agent\r\n"
    )
    .ok();
}

fn print_tree_meta(out: &mut impl Write, meta: &TreeMeta, index: usize) {
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
    write!(
        out,
        "  [{}] {} — {} ({})\r\n",
        index + 1,
        short_id,
        title,
        status
    )
    .ok();
}

/// Replay old entries as if they just happened — seamless, no "quit" feel.
fn replay_entries(out: &mut impl Write, entries: &[Entry]) {
    write!(out, "\x1b[?25l").ok();
    let mut in_turn = false;
    let mut state = RenderState::default();

    for entry in entries {
        match entry {
            Entry::SessionStart { .. } | Entry::SessionEnd { .. } | Entry::Label { .. } => continue,

            Entry::Message { message, .. }
                if message.role == agent_core::types::MessageRole::User =>
            {
                if in_turn {
                    write!(out, "\r\n").ok();
                }
                in_turn = true;

                let t = match &message.content {
                    agent_core::types::MessageContent::Text(t) => t.clone(),
                    _ => "[content blocks]".into(),
                };
                write!(out, "\r\n").ok();
                write!(
                    out,
                    "{}▸{} {}\r\n",
                    color::Fg(color::Green),
                    style::Reset,
                    t
                )
                .ok();
            }

            _ => {
                if !in_turn {
                    write!(
                        out,
                        "{}·  ·  ·{}\r\n",
                        color::Fg(color::LightBlack),
                        style::Reset
                    )
                    .ok();
                    in_turn = true;
                }
                render_event(out, &ServerEvent::Entry(entry.clone()), &mut state);
            }
        }
    }

    if let Some(last) = entries.last() {
        if !matches!(last, Entry::SessionEnd { .. }) && in_turn {
            // blank line before prompt
        }
    }
}


fn render_done(out: &mut impl Write, status: &str) {
    match status {
        "stop" | "complete" | "error" => {}
        "length" => {
            write!(out, "\r\n  {}⚠{} Stopped at length limit\r\n", color::Fg(color::Yellow), style::Reset).ok();
        }
        "aborted" => {
            write!(out, "\r\n  {}✖{} Aborted\r\n", color::Fg(color::Red), style::Reset).ok();
        }
        "cancelled" => {
            write!(out, "\r\n  {}✋{} Cancelled\r\n", color::Fg(color::Yellow), style::Reset).ok();
        }
        other => {
            print_warning(out, &format!("unknown completion status: {}", other));
        }
    }
}

fn print_entry_summary(out: &mut impl Write, entry: &Entry) {
    match entry {
        Entry::Message { message, .. } => {
            let role_str = match message.role {
                agent_core::types::MessageRole::User => "User",
                agent_core::types::MessageRole::Assistant => "Assistant",
                agent_core::types::MessageRole::System => "System",
                agent_core::types::MessageRole::Tool => "Tool",
            };
            let snippet = match &message.content {
                agent_core::types::MessageContent::Text(t) => {
                    if t.len() > 100 {
                        format!("{}...", &t[..100])
                    } else {
                        t.clone()
                    }
                }
                agent_core::types::MessageContent::Blocks(b) => format!("[{} blocks]", b.len()),
            };
            write!(out, "  {} ({}): {}\r\n", entry.id(), role_str, snippet).ok();
        }
        Entry::BashExec {
            command, exit_code, ..
        } => {
            write!(
                out,
                "  {} bash: {} (exit: {})\r\n",
                entry.id(),
                command,
                exit_code
            )
            .ok();
        }
        Entry::GoalSet { goal, .. } => {
            let _ = write!(out, "  {} 🎯 Goal: {}\r\n", entry.id(), goal);
        }
        Entry::ModelSet { model, .. } => {
            let _ = write!(out, "  {} 🤖 Model: {}\r\n", entry.id(), model);
        }
        Entry::SessionEnd {
            status, summary, ..
        } => {
            let s = summary.as_deref().unwrap_or("no summary");
            let _ = write!(
                out,
                "  {} 📝 Session end ({:?}): {}\r\n",
                entry.id(),
                status,
                s
            );
        }
        Entry::SessionStart { .. } => {
            let _ = write!(out, "  {} ▶ Session start\r\n", entry.id());
        }
        Entry::Label { label, .. } => {
            let _ = write!(out, "  {} 🏷 Label: {}\r\n", entry.id(), label);
        }
    }
}

// ── Event rendering ──

#[derive(Default)]
struct RenderState {
    _rendered: HashSet<String>,
    assistant_header_shown: bool,
    last_tool_args: Option<(String, serde_json::Value)>,
    in_thinking: bool,
    /// Consecutive \r\n pairs at the end of the most recent content write.
    /// Capped at 2. Used to avoid double blank lines when events each add
    /// a leading separator.
    trailing_newlines: u8,
    /// Current column position (chars since the last \n in output).
    /// Only TextChunk and ThinkingChunk update this; all other events end
    /// with \r\n so col is reset to 0 by the caller after those events.
    col: usize,
    spinner_shown: bool,
    spinner_added_newline: bool, // true when we injected \r\n before the spinner
    spinner_saved_col: usize,    // col at the time the spinner was drawn
}

/// Count trailing \r\n pairs in a string (max 2).
fn count_trailing_crlf(s: &str) -> u8 {
    let trimmed = s.trim_end_matches("\r\n");
    (s.len().saturating_sub(trimmed.len()) / 2).min(2) as u8
}

/// Write a blank-line separator, adding only as many \r\n as needed to reach
/// exactly one blank line (== 2 consecutive \r\n), given how many are already
/// at the end of the previous write.
fn write_blank_line(out: &mut impl Write, trailing: &mut u8) {
    let needed = 2u8.saturating_sub(*trailing);
    for _ in 0..needed {
        write!(out, "\r\n").ok();
    }
    *trailing = 2;
}

/// Normalize bare `\n` to `\r\n` for raw-mode terminal output.
fn normalize_for_raw(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

// ── Spinner ──

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_INTERVAL_MS: u64 = 80;

/// Update col after writing `text` (original content, not yet normalized).
fn update_col(col: &mut usize, text: &str) {
    match text.rfind('\n') {
        Some(pos) => *col = text[pos + 1..].chars().count(),
        None => *col += text.chars().count(),
    }
}

/// Draw the spinner on its own line.  If the cursor is mid-line (col > 0) we
/// emit a \r\n first so the spinner never shares a line with content.
fn draw_spinner(out: &mut impl Write, state: &mut RenderState, frame: char) {
    state.spinner_saved_col = state.col;
    if state.col > 0 {
        write!(out, "\r\n").ok();
        state.spinner_added_newline = true;
    } else {
        state.spinner_added_newline = false;
    }
    write!(out, "{}{}", style::Reset, frame).ok();
    state.spinner_shown = true;
    out.flush().ok();
}

/// Erase the spinner and reposition the cursor back to where content left off.
fn erase_spinner(out: &mut impl Write, state: &mut RenderState) {
    if !state.spinner_shown {
        return;
    }
    write!(out, "\r\x1b[2K").ok(); // clear spinner line, cursor at col 0
    if state.spinner_added_newline {
        write!(out, "\x1b[1A").ok(); // up to content line
        if state.spinner_saved_col > 0 {
            write!(out, "\x1b[{}C", state.spinner_saved_col).ok(); // right to saved col
        }
        state.col = state.spinner_saved_col;
    } else {
        state.col = 0;
    }
    if state.in_thinking {
        write!(out, "{}\x1b[2m", color::Fg(color::LightBlack)).ok();
    }
    state.spinner_shown = false;
    out.flush().ok();
}

/// Advance the spinner frame in place without touching the cursor row above.
fn tick_spinner(out: &mut impl Write, state: &mut RenderState, frame: char) {
    if !state.spinner_shown {
        return;
    }
    write!(out, "\r\x1b[2K{}{}", style::Reset, frame).ok();
    out.flush().ok();
}

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
                (Some(o), None)    => format!("{path}  {o}–"),
                (None,    Some(l)) => format!("{path}  1–{l}"),
                (None,    None)    => path.to_string(),
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

fn render_event(out: &mut impl Write, event: &ServerEvent, state: &mut RenderState) {
    match event {
        ServerEvent::TextChunk { content } => {
            if state.in_thinking {
                state.in_thinking = false;
                write!(out, "{}", style::Reset).ok();
                // Blank line between thinking and response; also suppress the
                // assistant_header \r\n below so we don't double-count.
                write_blank_line(out, &mut state.trailing_newlines);
                state.assistant_header_shown = true;
                state.col = 0;
            }
            if !state.assistant_header_shown {
                state.assistant_header_shown = true;
                write!(out, "\r\n").ok();
                state.trailing_newlines = 1;
                state.col = 0;
            }
            // Raw mode: `\n` alone leaves the cursor at the same column.
            // Normalize existing `\r\n` first (so we don't write `\r\r\n`), then
            // translate bare `\n` to `\r\n`.
            let normalized = normalize_for_raw(content);
            write!(out, "{}", normalized).ok();
            out.flush().ok();
            update_col(&mut state.col, content);
            let non_nl = !normalized.trim_end_matches("\r\n").is_empty();
            if non_nl {
                state.trailing_newlines = count_trailing_crlf(&normalized);
            } else {
                state.trailing_newlines =
                    state.trailing_newlines.saturating_add(count_trailing_crlf(&normalized)).min(2);
            }
        }
        ServerEvent::ToolStart { tool, input } => {
            state.last_tool_args = Some((tool.clone(), input.clone()));
        }
        ServerEvent::ToolResult { tool, exit, output } => {
            if state.in_thinking {
                state.in_thinking = false;
                write!(out, "{}", style::Reset).ok();
            }
            let args_str = state
                .last_tool_args
                .take()
                .map(|(_, input)| format_tool_args(tool, &input))
                .unwrap_or_default();
            write_blank_line(out, &mut state.trailing_newlines);
            write!(out, "  ⚙ {}{}{}", style::Bold, tool, style::Reset).ok();
            if !args_str.is_empty() {
                let args_display = args_str.replace('\n', "\r\n");
                write!(out, "  {}", args_display).ok();
            }
            let c = if *exit == 0 {
                color::Fg(color::LightBlack).to_string()
            } else {
                color::Fg(color::Red).to_string()
            };
            write!(out, "  (exit: {}{}{})\r\n", c, *exit, style::Reset).ok();
            if !output.is_empty() {
                write!(out, "{}", color::Fg(color::LightBlack)).ok();
                print_indented(out, output, "│");
                write!(out, "{}", style::Reset).ok();
            }
            state.trailing_newlines = 1; // print_indented / exit line ends with \r\n
        }
        ServerEvent::Entry(entry) => match entry {
            Entry::Message { message, .. }
                if message.role == agent_core::types::MessageRole::User =>
            {
                let t = match &message.content {
                    agent_core::types::MessageContent::Text(t) => t.clone(),
                    _ => "[content blocks]".into(),
                };
                write!(out, "\r\n").ok();
                write!(
                    out,
                    "{}▸{} {}\r\n",
                    color::Fg(color::Green),
                    style::Reset,
                    t
                )
                .ok();
            }
            Entry::GoalSet { goal, .. } => {
                write!(out, "\r\n").ok();
                write!(out, "🎯  {}\r\n", goal).ok();
            }
            Entry::ModelSet { model, .. } => {
                write!(out, "\r\n").ok();
                write!(out, "🤖  Model: {}\r\n", model).ok();
            }
            Entry::SessionEnd {
                summary, status, ..
            } => {
                write!(out, "\r\n").ok();
                let s = summary.as_deref().unwrap_or("");
                write!(
                    out,
                    "📝 {}Session ended ({:?}){}{}\r\n",
                    style::Bold,
                    status,
                    if s.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", s)
                    },
                    style::Reset
                )
                .ok();
            }
            Entry::Message { message, .. } => {
                let t = match &message.content {
                    agent_core::types::MessageContent::Text(t) => t.clone(),
                    _ => "[content blocks]".into(),
                };
                if !t.is_empty() {
                    let role_label = match message.role {
                        agent_core::types::MessageRole::Assistant => "Assistant",
                        agent_core::types::MessageRole::System => "System",
                        agent_core::types::MessageRole::Tool => "Tool",
                        _ => "",
                    };
                    write!(out, "\r\n").ok();
                    if !role_label.is_empty() {
                        write!(
                            out,
                            "{}  {}:{}\r\n",
                            color::Fg(color::Cyan),
                            role_label,
                            style::Reset
                        )
                        .ok();
                    }
                    write!(out, "{}\r\n", t).ok();
                }
            }
            Entry::BashExec {
                command,
                output,
                exit_code,
                ..
            } => {
                write!(out, "\r\n").ok();
                write!(
                    out,
                    "{}  🛠  {}bash: {}{}\r\n",
                    color::Fg(color::Yellow),
                    style::Bold,
                    command,
                    style::Reset
                )
                .ok();
                let c = if *exit_code == 0 {
                    color::Fg(color::Green).to_string()
                } else {
                    color::Fg(color::Red).to_string()
                };
                write!(out, "{}  bash (exit: {}){}\r\n", c, exit_code, style::Reset).ok();
                if !output.is_empty() {
                    print_indented(out, output, "│");
                }
            }
            _ => {}
        },
        ServerEvent::CapWarning { level, pct } => {
            print_warning(out, &format!("Context at {}% ({})", pct, level));
        }
        ServerEvent::Notification { level, message } => {
            if state.in_thinking {
                state.in_thinking = false;
                write!(out, "{}\r\n", style::Reset).ok();
                state.col = 0;
            }
            let text = normalize_for_raw(message);
            match level {
                NotificationLevel::Info => {
                    write!(out, "{}  {}{}\r\n", color::Fg(color::Yellow), text, style::Reset).ok();
                }
                NotificationLevel::Warning => {
                    write!(out, "{}  {}{}\r\n", color::Fg(color::Rgb(190, 90, 90)), text, style::Reset).ok();
                }
                NotificationLevel::Error => {
                    write!(out, "{}{}  Error: {}{}\r\n", color::Fg(color::Red), style::Bold, text, style::Reset).ok();
                }
                NotificationLevel::Fatal => {
                    write!(out, "{}{}  Fatal: {}{}\r\n", color::Fg(color::Red), style::Bold, text, style::Reset).ok();
                }
            }
        }
        ServerEvent::Diagnostics { source, files } => {
            if state.in_thinking {
                state.in_thinking = false;
                write!(out, "{}", style::Reset).ok();
            }
            use agent_core::types::DiagnosticSeverity;

            fn sev_color_label(sev: Option<DiagnosticSeverity>) -> (&'static str, &'static str) {
                match sev {
                    Some(DiagnosticSeverity::Error)       => ("\x1b[31m",    "error  "),
                    Some(DiagnosticSeverity::Warning)     => ("\x1b[33m",    "warning"),
                    Some(DiagnosticSeverity::Information) => ("\x1b[36m",    "info   "),
                    _                                     => ("\x1b[90m",    "hint   "),
                }
            }

            fn seen_summary(errors: u32, warnings: u32) -> String {
                match (errors, warnings) {
                    (0, 0) => String::new(),
                    (e, 0) => format!("{} seen error{}", e, if e == 1 { "" } else { "s" }),
                    (0, w) => format!("{} seen warning{}", w, if w == 1 { "" } else { "s" }),
                    (e, w) => format!("{} seen error{}, {} seen warning{}", e, if e == 1 { "" } else { "s" }, w, if w == 1 { "" } else { "s" }),
                }
            }

            let new_errors: usize = files.iter().flat_map(|f| &f.diagnostics)
                .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::Error))).count();
            let new_warnings: usize = files.iter().flat_map(|f| &f.diagnostics)
                .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::Warning))).count();

            let header_color = if new_errors > 0 { "\x1b[31m" }
                else if new_warnings > 0 { "\x1b[33m" }
                else { "\x1b[90m" };

            write_blank_line(out, &mut state.trailing_newlines);
            write!(out, "  {}◈\x1b[m {}\r\n", header_color, lang_display(source)).ok();

            for file in files {
                let display_path = std::path::Path::new(&file.path)
                    .file_name().and_then(|n| n.to_str()).unwrap_or(&file.path);

                write!(out, "    \x1b[1m{}\x1b[m\r\n", display_path).ok();

                let line_width = file.diagnostics.iter()
                    .map(|d| (d.range.start.line + 1).to_string().len())
                    .max().unwrap_or(1);
                for diag in &file.diagnostics {
                    let (col, label) = sev_color_label(diag.severity);
                    let first_line = diag.message.lines().next().unwrap_or("");
                    let msg: String = if first_line.chars().count() > 72 {
                        format!("{}…", first_line.chars().take(71).collect::<String>())
                    } else {
                        first_line.to_string()
                    };
                    write!(out, "      {}{}\x1b[m \x1b[90m{:>width$}\x1b[m  {}\r\n",
                        col, label, diag.range.start.line + 1, msg, width = line_width).ok();
                }

                let summary = seen_summary(file.seen_errors, file.seen_warnings);
                if !summary.is_empty() {
                    write!(out, "      \x1b[90m({})\x1b[m\r\n", summary).ok();
                }
            }
            state.trailing_newlines = 1;
        }
        ServerEvent::Done { status } => {
            if state.in_thinking {
                state.in_thinking = false;
                write!(out, "{}", style::Reset).ok();
            }
            if state.trailing_newlines == 0 {
                write!(out, "\r\n").ok();
            }
            render_done(out, status);
        }
        ServerEvent::FileChanged { path, kind } => {
            write!(out, "\r\n").ok();
            write!(out, "  📄 {} ({})\r\n", path, kind).ok();
        }
        ServerEvent::MetaUpdate { title } => {
            if let Some(t) = title {
                write!(out, "\r\n").ok();
                write!(out, "  {}Title: {}{}\r\n", style::Bold, t, style::Reset).ok();
            }
        }
        ServerEvent::ThinkingChunk { content } => {
            if !state.in_thinking {
                state.in_thinking = true;
                write!(out, "\r\n{}\x1b[2m", color::Fg(color::LightBlack)).ok();
                state.trailing_newlines = 0;
                state.col = 0;
            }
            let normalized = normalize_for_raw(content);
            write!(out, "{}", normalized).ok();
            out.flush().ok();
            update_col(&mut state.col, content);
            let non_nl = !normalized.trim_end_matches("\r\n").is_empty();
            if non_nl {
                state.trailing_newlines = count_trailing_crlf(&normalized);
            } else {
                state.trailing_newlines =
                    state.trailing_newlines.saturating_add(count_trailing_crlf(&normalized)).min(2);
            }
        }
    }
}

// ── Input line editor with history and multiline support ──

const MARGIN_WIDTH: usize = 2;

struct InputLine {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft: String,
    last_visual_line: usize,
    total_visual_lines: usize,
    anchored_col: Option<usize>,
}

impl InputLine {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
            last_visual_line: 0,
            total_visual_lines: 1,
            anchored_col: None,
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.last_visual_line = 0;
        self.total_visual_lines = 1;
        self.anchored_col = None;
    }

    fn line_info(&self) -> (usize, usize, usize) {
        let line_start = self.buf[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|pos| pos + 1)
            .unwrap_or(0);
        let line_end = self.buf[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|pos| self.cursor + pos)
            .unwrap_or(self.buf.len());
        let newlines_before = self.buf[..self.cursor]
            .iter()
            .filter(|&&c| c == '\n')
            .count();
        (line_start, line_end, newlines_before)
    }

    fn line_min(line_start: usize, buf_len: usize) -> usize {
        if line_start == 0 {
            0
        } else {
            (line_start + MARGIN_WIDTH).min(buf_len)
        }
    }

    fn snap_cursor(&mut self) {
        let (line_start, line_end, _) = self.line_info();
        let min = Self::line_min(line_start, self.buf.len());
        self.cursor = self.cursor.max(min).min(line_end);
    }

    fn handle_key(
        &mut self,
        key: Key,
        out: &mut impl Write,
        prompt: &str,
    ) -> LineEvent {
        match key {
            Key::Char('\n') | Key::Char('\r') => {
                let line: String = self.buf.iter().collect();
                if !line.is_empty() && self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                self.buf.clear();
                self.cursor = 0;
                self.history_idx = None;
                self.anchored_col = None;
                write!(out, "\r\n").ok();
                out.flush().ok();
                LineEvent::Submit(line)
            }
            Key::Alt('\n') => {
                self.buf.insert(self.cursor, '\n');
                self.cursor += 1;
                for _ in 0..MARGIN_WIDTH {
                    self.buf.insert(self.cursor, ' ');
                    self.cursor += 1;
                }
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    let (line_start, _, _) = self.line_info();
                    if line_start > 0 && self.cursor <= line_start + MARGIN_WIDTH {
                        for _ in 0..=MARGIN_WIDTH {
                            self.buf.remove(line_start - 1);
                        }
                        self.cursor = line_start - 1;
                    } else {
                        self.cursor -= 1;
                        self.buf.remove(self.cursor);
                    }
                    self.anchored_col = None;
                    self.redraw(out, prompt);
                }
                LineEvent::Continue
            }
            Key::Delete | Key::Ctrl('d') => {
                if self.cursor < self.buf.len() {
                    self.buf.remove(self.cursor);
                    self.anchored_col = None;
                    self.redraw(out, prompt);
                }
                LineEvent::Continue
            }
            Key::Left | Key::Ctrl('b') => {
                let (line_start, _, _) = self.line_info();
                if line_start > 0 && self.cursor <= line_start + MARGIN_WIDTH {
                    self.cursor = line_start.saturating_sub(1);
                } else if self.cursor > 0 {
                    self.cursor -= 1;
                }
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Right => {
                let (_, line_end, _) = self.line_info();
                if self.cursor == self.buf.len() {
                    for c in "\n".chars().chain(std::iter::repeat_n(' ', MARGIN_WIDTH)) {
                        self.buf.insert(self.cursor, c);
                        self.cursor += 1;
                    }
                } else if self.cursor >= line_end && line_end < self.buf.len() {
                    let next_line_start = line_end + 1;
                    self.cursor = Self::line_min(next_line_start, self.buf.len());
                } else {
                    self.cursor = self.buf.len().min(self.cursor + 1);
                }
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Ctrl('f') => {
                let (_, line_end, _) = self.line_info();
                if self.cursor >= line_end && line_end < self.buf.len() {
                    let next_line_start = line_end + 1;
                    self.cursor = Self::line_min(next_line_start, self.buf.len());
                } else {
                    self.cursor = self.buf.len().min(self.cursor + 1);
                }
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Home | Key::Ctrl('a') => {
                let (line_start, _, _) = self.line_info();
                self.cursor = Self::line_min(line_start, self.buf.len());
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::End | Key::Ctrl('e') => {
                let (_, line_end, _) = self.line_info();
                self.cursor = line_end;
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Up => {
                let (line_start, _, newlines_before) = self.line_info();
                let cur_col = self.cursor - line_start;
                let visual_col = if line_start > 0 {
                    cur_col.saturating_sub(MARGIN_WIDTH)
                } else {
                    cur_col
                };
                if self.anchored_col.is_none() {
                    self.anchored_col = Some(visual_col);
                }
                let want_visual = self.anchored_col.unwrap();
                if newlines_before == 0 {
                    self.anchored_col = None;
                    self.history_prev();
                } else {
                    let prev_line_end = line_start - 1;
                    let prev_line_start = self.buf[..prev_line_end]
                        .iter()
                        .rposition(|&c| c == '\n')
                        .map(|pos| pos + 1)
                        .unwrap_or(0);
                    let target = if prev_line_start > 0 {
                        prev_line_start + MARGIN_WIDTH + want_visual
                    } else {
                        want_visual
                    };
                    let min = Self::line_min(prev_line_start, self.buf.len());
                    self.cursor = target.min(prev_line_end).max(min);
                }
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Down => {
                let (line_start, line_end, newlines_before) = self.line_info();
                let cur_col = self.cursor - line_start;
                let visual_col = if line_start > 0 {
                    cur_col.saturating_sub(MARGIN_WIDTH)
                } else {
                    cur_col
                };
                if self.anchored_col.is_none() {
                    self.anchored_col = Some(visual_col);
                }
                let want_visual = self.anchored_col.unwrap();
                let total_nl = self.buf.iter().filter(|&&c| c == '\n').count();
                if newlines_before >= total_nl {
                    self.anchored_col = None;
                    self.history_next();
                } else {
                    let next_line_start = line_end + 1;
                    let next_line_end = self.buf[next_line_start..]
                        .iter()
                        .position(|&c| c == '\n')
                        .map(|pos| next_line_start + pos)
                        .unwrap_or(self.buf.len());
                    let target = next_line_start + MARGIN_WIDTH + want_visual;
                    let min = Self::line_min(next_line_start, self.buf.len());
                    self.cursor = target.min(next_line_end).max(min);
                }
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Ctrl('p') => {
                self.anchored_col = None;
                self.history_prev();
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Ctrl('n') => {
                self.anchored_col = None;
                self.history_next();
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Ctrl('u') => {
                let (line_start, _, _) = self.line_info();
                let kill_start = Self::line_min(line_start, self.buf.len());
                self.buf.drain(kill_start..self.cursor);
                self.cursor = kill_start;
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Ctrl('k') => {
                self.buf.truncate(self.cursor);
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            Key::Ctrl('w') => {
                if self.cursor > 0 {
                    let mut end = self.cursor;
                    while end > 0 && self.buf[end - 1] == ' ' {
                        end -= 1;
                    }
                    while end > 0 && self.buf[end - 1] != ' ' {
                        end -= 1;
                    }
                    let count = self.cursor - end;
                    for _ in 0..count {
                        self.buf.remove(end);
                    }
                    self.cursor = end;
                    self.anchored_col = None;
                    self.redraw(out, prompt);
                }
                LineEvent::Continue
            }
            Key::Ctrl('c') => LineEvent::Quit,
            Key::Char(c) => {
                self.buf.insert(self.cursor, c);
                self.cursor += 1;
                self.anchored_col = None;
                self.redraw(out, prompt);
                LineEvent::Continue
            }
            _ => LineEvent::Continue,
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.draft = self.buf.iter().collect();
                self.history_idx = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(ref mut i) => *i -= 1,
        }
        if let Some(i) = self.history_idx {
            self.buf = self.history[i].chars().collect();
            self.cursor = self.buf.len();
        }
    }

    fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 >= self.history.len() => {
                self.history_idx = None;
                self.buf = self.draft.chars().collect();
                self.cursor = self.buf.len();
            }
            Some(ref mut i) => {
                *i += 1;
                let idx = *i;
                self.buf = self.history[idx].chars().collect();
                self.cursor = self.buf.len();
            }
        }
    }

    fn redraw(&mut self, out: &mut impl Write, prompt: &str) {
        self.snap_cursor();

        let old_total = self.total_visual_lines;

        if self.last_visual_line > 0 {
            write!(out, "{}", termion::cursor::Up(self.last_visual_line as u16)).ok();
        }
        write!(out, "\r{}", clear::AfterCursor).ok();

        write!(out, "{}{}{}", color::Fg(color::Yellow), prompt, style::Reset).ok();
        let content: String = self.buf.iter().collect();
        write!(out, "{}", content.replace('\n', "\r\n")).ok();

        self.total_visual_lines = 1 + self.buf.iter().filter(|&&c| c == '\n').count();

        write!(out, "\r").ok();
        for _ in self.total_visual_lines..old_total {
            write!(out, "\r\n{}", clear::AfterCursor).ok();
        }

        let cursor_row = if old_total > self.total_visual_lines {
            old_total - 1
        } else {
            self.total_visual_lines - 1
        };

        let newlines_before = self.buf[..self.cursor]
            .iter()
            .filter(|&&c| c == '\n')
            .count();
        let line_start = self.buf[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|pos| pos + 1)
            .unwrap_or(0);
        let col_in_line = self.cursor - line_start;
        let cursor_line = newlines_before;
        self.last_visual_line = cursor_line;

        let rows_to_move = cursor_row - cursor_line;
        if rows_to_move > 0 {
            write!(out, "{}", termion::cursor::Up(rows_to_move as u16)).ok();
        }

        let prompt_width = if cursor_line == 0 { prompt.len() } else { 0 };
        if prompt_width + col_in_line > 0 {
            write!(
                out,
                "{}",
                termion::cursor::Right((prompt_width + col_in_line) as u16)
            )
            .ok();
        }

        out.flush().ok();
    }
}

enum LineEvent {
    Continue,
    Submit(String),
    Quit,
}

// ── Tree selection ──

fn select_or_create_tree(
    input_line: &mut InputLine,
    keys: &mut impl Iterator<Item = Result<Key, std::io::Error>>,
    out: &mut impl Write,
    backend: &Backend,
) -> Result<String, String> {
    loop {
        let trees = backend.list_trees()?;

        if !trees.is_empty() {
            write!(out, "\r\nYour trees:\r\n").ok();
            for (i, tree) in trees.iter().enumerate() {
                print_tree_meta(out, tree, i);
            }
            write!(out, "\r\n").ok();
            write!(out, "Select a tree (number), 'new', or 'q' to quit: ").ok();
            out.flush().ok();

            input_line.clear();
            let result = loop {
                match keys.next() {
                    Some(Ok(k)) => match input_line.handle_key(k, out, "") {
                        LineEvent::Continue => {}
                        ev => break ev,
                    },
                    Some(Err(_)) | None => break LineEvent::Quit,
                }
            };
            let input = match result {
                LineEvent::Submit(s) => s.trim().to_lowercase(),
                LineEvent::Quit => std::process::exit(0),
                LineEvent::Continue => String::new(),
            };

            if input == "q" || input == "quit" {
                std::process::exit(0);
            }
            if input == "new" {
                return create_tree_interactive(input_line, keys, out, backend);
            }

            if let Ok(idx) = input.parse::<usize>() {
                if idx > 0 && idx <= trees.len() {
                    return Ok(trees[idx - 1].id.clone());
                }
            }

            if !input.is_empty() {
                let matches: Vec<&TreeMeta> =
                    trees.iter().filter(|t| t.id.starts_with(&input)).collect();
                if matches.len() == 1 {
                    return Ok(matches[0].id.clone());
                }
                if matches.len() > 1 {
                    write!(out, "Multiple matches, be more specific.\r\n").ok();
                    continue;
                }
            }

            write!(out, "Invalid selection.\r\n").ok();
        } else {
            write!(out, "No trees found. Let's create one.\r\n").ok();
            return create_tree_interactive(input_line, keys, out, backend);
        }
    }
}

fn create_tree_interactive(
    input_line: &mut InputLine,
    keys: &mut impl Iterator<Item = Result<Key, std::io::Error>>,
    out: &mut impl Write,
    backend: &Backend,
) -> Result<String, String> {
    let mut input_text = |prompt: &str| -> String {
        write!(out, "{}", prompt).ok();
        out.flush().ok();
        input_line.clear();
        let result = loop {
            match keys.next() {
                Some(Ok(k)) => match input_line.handle_key(k, out, "") {
                    LineEvent::Continue => {}
                    ev => break ev,
                },
                Some(Err(_)) | None => break LineEvent::Quit,
            }
        };
        match result {
            LineEvent::Submit(s) => s,
            _ => String::new(),
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
    )?;
    let short_id = if meta.id.len() > 8 {
        &meta.id[..8]
    } else {
        &meta.id
    };
    write!(
        out,
        "{}Created tree {} ({}){}\r\n",
        color::Fg(color::Green),
        short_id,
        meta.title.as_deref().unwrap_or("untitled"),
        style::Reset
    )
    .ok();
    Ok(meta.id)
}

// ── Message processing ──

fn poll_key() -> Option<Key> {
    let stdin = io::stdin();
    let mut poll_fd = PollFd::new(stdin.as_fd(), PollFlags::POLLIN);

    match poll(std::slice::from_mut(&mut poll_fd), PollTimeout::from(0u16)) {
        Ok(n) if n > 0 => {
            let mut buf = [0u8; 1];
            match io::stdin().read(&mut buf) {
                Ok(1) => match buf[0] {
                    0x1b => Some(Key::Esc),
                    0x03 => Some(Key::Ctrl('c')),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

fn process_message(
    backend: &Backend,
    tree_id: &str,
    text: &str,
    out: &mut impl Write,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut session = backend.connect_session(tree_id)?;
    let ws_fd = session.as_raw_fd().ok_or("no fd on WS socket")?;
    let stdin_fd = std::io::stdin().as_raw_fd();
    session.set_nonblocking(true)?;
    session.send_message(text)?;

    write!(out, "\x1b[?25l").ok();

    let mut state = RenderState::default();
    let mut cancel_signalled = false;
    let mut spin_frame = 0usize;
    let mut last_spin = Instant::now();

    draw_spinner(out, &mut state, SPINNER_FRAMES[spin_frame]);

    loop {
        // 1. Drain WS events
        loop {
            match session.try_next_event() {
                TryEvent::Event(ev) => {
                    if matches!(&ev, ServerEvent::Entry(_)) {
                        continue;
                    }
                    erase_spinner(out, &mut state);
                    let done = matches!(&ev, ServerEvent::Done { .. });
                    // INTENTIONAL: fatal errors must exit the loop just like Done.
                    // Without this the dead worker never sends Done and the loop
                    // spins forever, leaving the terminal frozen with no exit.
                    let fatal = matches!(&ev, ServerEvent::Notification { level: NotificationLevel::Fatal, .. });
                    render_event(out, &ev, &mut state);
                    // All events except streaming chunks always end with \r\n.
                    // ToolStart writes nothing so col is unchanged.
                    if !matches!(
                        &ev,
                        ServerEvent::TextChunk { .. }
                            | ServerEvent::ThinkingChunk { .. }
                            | ServerEvent::ToolStart { .. }
                    ) {
                        state.col = 0;
                    }
                    if done || fatal {
                        return Ok(());
                    }
                    draw_spinner(out, &mut state, SPINNER_FRAMES[spin_frame]);
                    continue;
                }
                TryEvent::Closed => {
                    erase_spinner(out, &mut state);
                    return Ok(());
                }
                TryEvent::Err(e) => {
                    erase_spinner(out, &mut state);
                    write!(out, "\r\nws error: {}\r\n", e).ok();
                    return Ok(());
                }
                TryEvent::WouldBlock => break,
            }
        }

        // 2. Check Ctrl-C from the outer signal handler
        if stop.load(Ordering::Relaxed) {
            erase_spinner(out, &mut state);
            write!(out, "\r\nInterrupted\r\n").ok();
            break;
        }

        // 3. Peek stdin for Esc or Ctrl-C
        if !cancel_signalled {
            if let Some(key) = poll_key() {
                if matches!(key, Key::Esc | Key::Ctrl('c')) {
                    erase_spinner(out, &mut state);
                    write!(
                        out,
                        "\r\n  {}⏸ Cancelling…{}\r\n",
                        color::Fg(color::Yellow),
                        style::Reset
                    )
                    .ok();
                    out.flush().ok();
                    let _ = session.send_stop();
                    cancel_signalled = true;
                    state.col = 0;
                    draw_spinner(out, &mut state, SPINNER_FRAMES[spin_frame]);
                }
            }
        }

        // 4. Animate spinner
        if last_spin.elapsed() >= Duration::from_millis(SPINNER_INTERVAL_MS) {
            spin_frame = (spin_frame + 1) % SPINNER_FRAMES.len();
            tick_spinner(out, &mut state, SPINNER_FRAMES[spin_frame]);
            last_spin = Instant::now();
        }

        // Block until WS data arrives or stdin is ready, waking at least once per
        // spinner frame so the spinner stays smooth. This replaces the former 20ms
        // sleep; the socket becoming readable is what actually wakes us up when a
        // token arrives, so display latency drops from up to 20ms to near-zero.
        // INTENTIONAL: do not remove this poll or replace it with a bare sleep.
        let elapsed = last_spin.elapsed().as_millis() as u64;
        let wait_ms = SPINNER_INTERVAL_MS.saturating_sub(elapsed);
        let timeout = PollTimeout::try_from(wait_ms).unwrap_or(PollTimeout::ZERO);
        let mut fds = unsafe {
            [
                PollFd::new(BorrowedFd::borrow_raw(ws_fd), PollFlags::POLLIN),
                PollFd::new(BorrowedFd::borrow_raw(stdin_fd), PollFlags::POLLIN),
            ]
        };
        let _ = poll(&mut fds, timeout);
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
    let mut out = io::stdout()
        .into_raw_mode()
        .map_err(|e| format!("raw mode: {}", e))?;
    let mut keys = io::stdin().keys();

    write!(
        out,
        "{}Connected{}",
        color::Fg(color::Green),
        style::Reset
    )
    .ok();
    print_help(&mut out);
    write!(out, "\r\n").ok();

    let mut input_line = InputLine::new();
    let mut current_tree_id = if let Some(rp) = initial_repo_path {
        let meta = backend
            .create_tree(Some("untitled"), Some(&rp), None, &[], None, &[], &[])
            .map_err(|e| format!("failed to create tree: {}", e))?;
        let sid = if meta.id.len() > 8 {
            &meta.id[..8]
        } else {
            &meta.id
        };
        write!(out, "Created tree {} in {}\r\n", sid, rp).ok();
        meta.id
    } else {
        select_or_create_tree(&mut input_line, &mut keys, &mut out, backend)?
    };
    let mut show_header = true;

    loop {
        if stop.load(Ordering::Relaxed) {
            write!(out, "\r\nInterrupted\r\n").ok();
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
                    write!(
                        out,
                        "\r\n{}{}{}  {}·  {}{}\r\n",
                        style::Bold,
                        title,
                        style::Reset,
                        color::Fg(color::LightBlack),
                        short_id,
                        style::Reset
                    )
                    .ok();

                    if let Ok(entries) = backend.get_entries(&current_tree_id) {
                        if !entries.is_empty() {
                            let last: Vec<_> =
                                entries.iter().rev().take(10).rev().cloned().collect();
                            replay_entries(&mut out, &last);
                        }
                    }
                }
                Err(e) => print_warning(&mut out, &format!("Failed to load tree: {}", e)),
            }
            show_header = false;
        }

        write!(out, "\r\n\x1b[?25h{}>{} ", color::Fg(color::Yellow), style::Reset).ok();
        out.flush().ok();

        input_line.clear();
        let result = loop {
            match keys.next() {
                Some(Ok(k)) => match input_line.handle_key(k, &mut out, "> ") {
                    LineEvent::Continue => {}
                    ev => break ev,
                },
                Some(Err(_)) | None => break LineEvent::Quit,
            }
        };
        let input = match result {
            LineEvent::Submit(s) => s,
            LineEvent::Quit => {
                write!(out, "Goodbye!\r\n").ok();
                break;
            }
            LineEvent::Continue => String::new(),
        };
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }

        match parse_input(&input) {
            CliCommand::Quit => {
                write!(out, "Goodbye!\r\n").ok();
                break;
            }
            CliCommand::Help => print_help(&mut out),
            CliCommand::ListTrees => match backend.list_trees() {
                Ok(trees) => {
                    write!(out, "\r\n").ok();
                    for (i, t) in trees.iter().enumerate() {
                        print_tree_meta(&mut out, t, i);
                    }
                }
                Err(e) => print_error(&mut out, &format!("Failed to list trees: {}", e)),
            },
            CliCommand::Create {
                title,
                repo_path,
                model,
            } => {
                match backend.create_tree(
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
                        write!(
                            out,
                            "{}Created tree {} ({}){}\r\n",
                            color::Fg(color::Green),
                            short_id,
                            meta.title.as_deref().unwrap_or("untitled"),
                            style::Reset
                        )
                        .ok();
                    }
                    Err(e) => print_error(&mut out, &format!("Failed to create tree: {}", e)),
                }
            }
            CliCommand::Switch(id) => {
                if id.is_empty() {
                    print_error(&mut out, "Usage: /switch <tree_id>");
                    continue;
                }
                match backend.get_tree(&id) {
                    Ok(meta) => {
                        current_tree_id = meta.id;
                        show_header = true;
                        write!(
                            out,
                            "{}Switched to tree {}{}\r\n",
                            color::Fg(color::Green),
                            meta.title.as_deref().unwrap_or(&id),
                            style::Reset
                        )
                        .ok();
                    }
                    Err(e) => print_error(&mut out, &format!("Tree not found: {}", e)),
                }
            }
            CliCommand::Stop => match backend.stop_agent(&current_tree_id) {
                Ok(()) => {
                    let _ = write!(
                        out,
                        "{}Stop signaled{}\r\n",
                        color::Fg(color::Yellow),
                        style::Reset
                    );
                }
                Err(e) => print_error(&mut out, &format!("Failed to stop: {}", e)),
            },
            CliCommand::Show => match backend.get_tree(&current_tree_id) {
                Ok(meta) => {
                    write!(out, "{}Tree info:{}\r\n", style::Bold, style::Reset).ok();
                    write!(out, "  ID:        {}\r\n", meta.id).ok();
                    write!(
                        out,
                        "  Title:     {}\r\n",
                        meta.title.as_deref().unwrap_or("(none)")
                    )
                    .ok();
                    write!(
                        out,
                        "  Repo path: {}\r\n",
                        meta.repo_path
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or("(none)".into())
                    )
                    .ok();
                    write!(
                        out,
                        "  Active:    {}\r\n",
                        if meta.leaf_id.is_some() { "yes" } else { "no" }
                    )
                    .ok();
                }
                Err(e) => print_error(&mut out, &format!("Failed to load tree: {}", e)),
            },
            CliCommand::Entries(n) => {
                let limit = n.unwrap_or(10);
                match backend.get_entries(&current_tree_id) {
                    Ok(entries) => {
                        let last: Vec<_> = entries.iter().rev().take(limit).rev().collect();
                        write!(
                            out,
                            "{}Last {} entries:{}\r\n",
                            style::Bold,
                            last.len(),
                            style::Reset
                        )
                        .ok();
                        for e in &last {
                            print_entry_summary(&mut out, e);
                        }
                    }
                    Err(e) => print_error(&mut out, &format!("Failed to load entries: {}", e)),
                }
            }
            CliCommand::Message(text) => {
                write!(out, "\x1b[?25l").ok();
                out.flush().ok();
                process_message(backend, &current_tree_id, &text, &mut out, stop)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_for_raw_replaces_bare_newlines() {
        let input = "hello\nworld\n";
        let result = normalize_for_raw(input);
        assert_eq!(result, "hello\r\nworld\r\n");
    }

    #[test]
    fn test_normalize_for_raw_no_double_carriage_return() {
        let input = "hello\r\nworld\r\n";
        let result = normalize_for_raw(input);
        assert_eq!(result, "hello\r\nworld\r\n");
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
        assert!(output.contains('✖'), "aborted should show ✖, got: {output}");
        assert!(output.contains("Aborted"), "aborted should show Aborted, got: {output}");
    }

    #[test]
    fn test_render_done_cancelled() {
        let mut buf = Vec::new();
        render_done(&mut buf, "cancelled");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('✋'), "cancelled should show ✋, got: {output}");
        assert!(output.contains("Cancelled"), "cancelled should show Cancelled, got: {output}");
        assert!(!output.contains("Done"), "cancelled should not show Done, got: {output}");
    }

    #[test]
    fn test_render_done_length() {
        let mut buf = Vec::new();
        render_done(&mut buf, "length");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('⚠'), "length should show ⚠, got: {output}");
        assert!(output.contains("Stopped at length limit"), "length should show warning, got: {output}");
    }

    #[test]
    fn test_normalize_for_raw_no_newlines() {
        let input = "hello world";
        let result = normalize_for_raw(input);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_normalize_for_raw_mixed() {
        let input = "a\r\nb\nc\r\nd\n";
        let result = normalize_for_raw(input);
        assert_eq!(result, "a\r\nb\r\nc\r\nd\r\n");
    }

    #[test]
    fn test_inputline_backspace_at_start_is_noop() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        il.handle_key(Key::Backspace, &mut buf, "> ");
        assert!(il.buf.is_empty());
    }

    #[test]
    fn test_inputline_cursor_movement() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Left, &mut buf, "> ");
        il.handle_key(Key::Left, &mut buf, "> ");
        il.handle_key(Key::Char('X'), &mut buf, "> ");
        let result: String = il.buf.iter().collect();
        assert_eq!(result, "helXlo");
    }

    #[test]
    fn test_inputline_history_cycle() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();

        for c in "first".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ");
        assert!(matches!(result, LineEvent::Submit(s) if s == "first"));

        for c in "second".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ");
        assert!(matches!(result, LineEvent::Submit(s) if s == "second"));

        il.handle_key(Key::Up, &mut buf, "> ");
        il.handle_key(Key::Up, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "first");

        il.handle_key(Key::Down, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "second");

        il.handle_key(Key::Down, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "");
    }

    #[test]
    fn test_inputline_alt_enter_inserts_newline() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        for c in "world".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ");
        assert!(matches!(result, LineEvent::Submit(s) if s == "hello\n  world"));
    }

    #[test]
    fn test_inputline_right_arrow_at_end_inserts_newline() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Right, &mut buf, "> ");
        for c in "world".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ");
        assert!(matches!(result, LineEvent::Submit(s) if s == "hello\n  world"));
    }

    #[test]
    fn test_inputline_alt_enter_auto_indent() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "  hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        for c in "world".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ");
        assert!(matches!(result, LineEvent::Submit(s) if s == "  hello\n  world"));
    }

    #[test]
    fn test_inputline_right_arrow_at_end_auto_indent() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "  hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Right, &mut buf, "> ");
        for c in "world".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ");
        assert!(matches!(result, LineEvent::Submit(s) if s == "  hello\n  world"));
    }

    #[test]
    fn test_inputline_right_arrow_mid_line_moves_cursor() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "ab".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Left, &mut buf, "> ");
        // cursor at 1, right arrow should move to 2 (not insert newline)
        il.handle_key(Key::Right, &mut buf, "> ");
        assert_eq!(il.cursor, 2);
        assert_eq!(il.buf.iter().collect::<String>(), "ab");
    }

    #[test]
    fn test_inputline_backspace_joins_lines_at_newline() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "abc".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        // buf is now "abc\n  ", cursor at end.
        // One backspace removes the \n + 2-space margin, joining to "abc".
        il.handle_key(Key::Backspace, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "abc");
        assert_eq!(il.cursor, 3);
    }

    #[test]
    fn test_inputline_ctrl_u_kills_to_start() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Left, &mut buf, "> ");
        il.handle_key(Key::Left, &mut buf, "> ");
        il.handle_key(Key::Ctrl('u'), &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "lo");
        assert_eq!(il.cursor, 0);
    }

    #[test]
    fn test_inputline_ctrl_k_kills_to_end() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ");
        }
        il.handle_key(Key::Left, &mut buf, "> ");
        il.handle_key(Key::Left, &mut buf, "> ");
        il.handle_key(Key::Ctrl('k'), &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "hel");
    }

    // ── New tests for Step 7 ──

    #[test]
    fn test_up_down_2d_basic() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Build "abc\n  def" — len 9
        for c in "abc".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        for c in "def".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        // cursor at end of buffer (position 9)
        assert_eq!(il.cursor, 9);
        // Up → prev_line_end (the '\n' at position 3), cursor clips to 3
        il.handle_key(Key::Up, &mut buf, "> ");
        assert_eq!(il.cursor, 3);
        // Down → back to end of buffer
        il.handle_key(Key::Down, &mut buf, "> ");
        assert_eq!(il.cursor, 9);
    }

    #[test]
    fn test_up_down_anchored_col() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Build "hello\n  hi" — first line has 5 chars, second has 2 chars
        for c in "hello".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        for c in "hi".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        // cursor at end of buffer (position 10), visual col = (10-6)-2 = 2
        assert_eq!(il.cursor, 10);
        // Up → visual col 2 on first line = position 2
        il.handle_key(Key::Up, &mut buf, "> ");
        assert_eq!(il.cursor, 2);
        // Down → back to end of buffer (visual col 2 on second line = 6+2+2 = 10)
        il.handle_key(Key::Down, &mut buf, "> ");
        assert_eq!(il.cursor, 10);
        // Up again → restores visual col 2 on first line = position 2
        il.handle_key(Key::Up, &mut buf, "> ");
        assert_eq!(il.cursor, 2);
    }

    #[test]
    fn test_up_at_first_row_goes_to_history() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Submit "bar" to history
        for c in "bar".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Char('\n'), &mut buf, "> ");
        // Single-line buffer "foo"
        for c in "foo".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        // Up on first row → history_prev → loads "bar"
        il.handle_key(Key::Up, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "bar");
    }

    #[test]
    fn test_down_at_last_row_goes_to_history() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Submit "first" to history
        for c in "first".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Char('\n'), &mut buf, "> ");
        // Build multiline: "a\n  b"
        il.handle_key(Key::Char('a'), &mut buf, "> ");
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        il.handle_key(Key::Char('b'), &mut buf, "> ");
        // Up twice: first 2D to first row, then history → "first"
        il.handle_key(Key::Up, &mut buf, "> ");
        il.handle_key(Key::Up, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "first");
        // Down returns to draft "a\n  b"
        il.handle_key(Key::Down, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "a\n  b");
        // Down again on last row does nothing (already on live draft)
        il.handle_key(Key::Down, &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "a\n  b");
    }

    #[test]
    fn test_ctrl_pn_always_history() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Submit "prev" to history
        for c in "prev".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Char('\n'), &mut buf, "> ");
        // Multiline buffer: "a\n  b"
        il.handle_key(Key::Char('a'), &mut buf, "> ");
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        il.handle_key(Key::Char('b'), &mut buf, "> ");
        // Ctrl('p') always history — even on middle row
        il.handle_key(Key::Ctrl('p'), &mut buf, "> ");
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "prev");
    }

    #[test]
    fn test_end_stops_before_newline() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Build "ab\n  cd" — \n at position 2
        for c in "ab".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        for c in "cd".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        // Set cursor to line start (position 0), then End → stops at line_end (the \n at pos 2)
        il.cursor = 0;
        il.handle_key(Key::End, &mut buf, "> ");
        assert_eq!(il.cursor, 2, "End should stop before newline, not at buf.len()");
    }

    #[test]
    fn test_anchor_resets_on_left() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        // Build "hello\n  world"
        for c in "hello".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ");
        for c in "world".chars() { il.handle_key(Key::Char(c), &mut buf, "> "); }
        // cursor at end (pos 13), visual col = (13-6)-2 = 5, Up lands at pos 5 ('\n')
        il.handle_key(Key::Up, &mut buf, "> ");
        assert_eq!(il.cursor, 5);
        // Left resets anchor, moves to pos 4
        il.handle_key(Key::Left, &mut buf, "> ");
        assert_eq!(il.cursor, 4);
        // Down from new visual col 4 on first line → visual col 4 on second line: 6+2+4=12
        il.handle_key(Key::Down, &mut buf, "> ");
        assert_eq!(il.cursor, 12);
    }

    #[test]
    fn test_render_tool_suppresses_start() {
        let mut buf = Vec::new();
        let mut state = RenderState::default();
        render_event(
            &mut buf,
            &ServerEvent::ToolStart {
                tool: "bash".into(),
                input: serde_json::json!({"command":"echo hi","description":"test"}),
            },
            &mut state,
        );
        render_event(
            &mut buf,
            &ServerEvent::ToolResult {
                tool: "bash".into(),
                exit: 0,
                output: "hi".into(),
            },
            &mut state,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("hi"),
            "output should contain 'hi', got: {output}"
        );
        assert!(
            !output.contains("ToolStart"),
            "output should not contain 'ToolStart', got: {output}"
        );
    }

    #[test]
    fn test_render_no_assistant_header() {
        let mut buf = Vec::new();
        let mut state = RenderState::default();
        render_event(
            &mut buf,
            &ServerEvent::TextChunk {
                content: "hello world".into(),
            },
            &mut state,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("hello world"),
            "output should contain chunk text, got: {output}"
        );
        assert!(
            !output.contains("Assistant"),
            "output should not contain 'Assistant', got: {output}"
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

    #[test]
    fn test_render_thinking_chunk_faint() {
        let mut buf = Vec::new();
        let mut state = RenderState::default();
        render_event(
            &mut buf,
            &ServerEvent::ThinkingChunk {
                content: "think".into(),
            },
            &mut state,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b[2m"), "output should contain faint marker");
        assert!(output.contains("think"), "output should contain 'think'");
        assert!(state.in_thinking);
    }

    #[test]
    fn test_render_thinking_resets_on_text() {
        let mut buf = Vec::new();
        let mut state = RenderState::default();
        render_event(
            &mut buf,
            &ServerEvent::ThinkingChunk {
                content: "think".into(),
            },
            &mut state,
        );
        render_event(
            &mut buf,
            &ServerEvent::TextChunk {
                content: "answer".into(),
            },
            &mut state,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b[m"), "output should contain reset, got: {output:?}");
        assert!(output.contains("think"), "output should contain 'think', got: {output:?}");
        assert!(output.contains("answer"), "output should contain 'answer', got: {output:?}");
        assert!(!state.in_thinking);
    }
}

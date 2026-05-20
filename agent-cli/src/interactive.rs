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
use std::os::unix::io::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use termion::event::Key;
use termion::input::TermRead;
use termion::raw::IntoRawMode;
use termion::{clear, color, style};

use agent_core::types::{Entry, ServerEvent, TreeMeta};

use crate::client::TryEvent;

use crate::client::AgentClient;

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
    let mut in_turn = false;
    let mut state = RenderState::default();

    for entry in entries {
        match entry {
            Entry::SessionStart { .. } | Entry::SessionEnd { .. } | Entry::Label { .. } => continue,

            Entry::Message { message, .. }
                if message.role == agent_core::types::MessageRole::User =>
            {
                if in_turn {
                    write!(
                        out,
                        "{}──────────────────────{}\r\n",
                        color::Fg(color::Blue),
                        style::Reset
                    )
                    .ok();
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
            write!(
                out,
                "{}──────────────────────{}\r\n",
                color::Fg(color::Blue),
                style::Reset
            )
            .ok();
        }
    }
}

fn render_done(out: &mut impl Write, status: &str) {
    let _ = match status {
        // Provider's "stop" finish_reason = model decided it was done.
        // "complete" is reserved for synthetic completion paths.
        // Both are happy-path turn endings.
        "stop" | "complete" => {
            write!(
                out,
                "\r\n  {}✓{} Done\r\n",
                color::Fg(color::Green),
                style::Reset
            )
        }
        // Model hit the provider's max_tokens or our hard cap.
        "length" => write!(
            out,
            "\r\n  {}⚠{} Stopped at length limit\r\n",
            color::Fg(color::Yellow),
            style::Reset
        ),
        // Worker crashed or was killed mid-turn.
        "aborted" => write!(
            out,
            "\r\n  {}✖{} Aborted\r\n",
            color::Fg(color::Red),
            style::Reset
        ),
        // User pressed Esc during the turn.
        "cancelled" => write!(
            out,
            "\r\n  {}✋{} Cancelled\r\n",
            color::Fg(color::Yellow),
            style::Reset
        ),
        // Unknown status — show it so we notice in testing.
        other => write!(
            out,
            "\r\n  {}■{} Done ({}){}\r\n",
            color::Fg(color::Yellow),
            style::Reset,
            other,
            style::Reset
        ),
    };
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
}

/// Normalize bare `\n` to `\r\n` for raw-mode terminal output.
fn normalize_for_raw(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn format_tool_args(tool: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let pick = match tool {
        "bash" => obj.get("command"),
        "read" | "write" | "edit" => obj.get("file_path").or_else(|| obj.get("path")),
        "find" => obj.get("pattern").or_else(|| obj.get("path")),
        "grep" => obj.get("pattern"),
        "git" => obj.get("command").or_else(|| obj.get("args")),
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
            if !state.assistant_header_shown {
                state.assistant_header_shown = true;
                write!(out, "\r\n").ok();
            }
            // Raw mode: `\n` alone leaves the cursor at the same column.
            // Normalize existing `\r\n` first (so we don't write `\r\r\n`), then
            // translate bare `\n` to `\r\n`.
            write!(out, "{}", normalize_for_raw(content)).ok();
            out.flush().ok();
        }
        ServerEvent::ToolStart { tool, input } => {
            state.last_tool_args = Some((tool.clone(), input.clone()));
        }
        ServerEvent::ToolResult { tool, exit, output } => {
            let args_str = state
                .last_tool_args
                .take()
                .map(|(_, input)| format_tool_args(tool, &input))
                .unwrap_or_default();
            write!(out, "\r\n  ⚙ {}{}{}", style::Bold, tool, style::Reset).ok();
            if !args_str.is_empty() {
                write!(out, "  {}", args_str).ok();
            }
            let c = if *exit == 0 {
                color::Fg(color::LightBlack).to_string()
            } else {
                color::Fg(color::Red).to_string()
            };
            write!(out, "  (exit: {}{}{})\r\n", c, *exit, style::Reset).ok();
            if !output.is_empty() {
                print_indented(out, output, "│");
            }
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
        ServerEvent::Error { message, fatal } => {
            if *fatal {
                print_error(out, &format!("Fatal: {}", message));
            } else {
                print_warning(out, &format!("Error: {}", message));
            }
        }
        ServerEvent::Done { status } => render_done(out, status),
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
    }
}

// ── Input line editor with history and multiline support ──

struct InputLine {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft: String,
}

impl InputLine {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_idx = None;
    }

    fn handle_key(
        &mut self,
        key: Key,
        out: &mut impl Write,
        prompt: &str,
        prev_lines: &mut usize,
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
                write!(out, "\r\n").ok();
                out.flush().ok();
                LineEvent::Submit(line)
            }
            Key::Alt('\n') => {
                self.buf.insert(self.cursor, '\n');
                self.cursor += 1;
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buf.remove(self.cursor);
                    self.redraw(out, prompt, prev_lines);
                }
                LineEvent::Continue
            }
            Key::Delete | Key::Ctrl('d') => {
                if self.cursor < self.buf.len() {
                    self.buf.remove(self.cursor);
                    self.redraw(out, prompt, prev_lines);
                }
                LineEvent::Continue
            }
            Key::Left | Key::Ctrl('b') => {
                self.cursor = self.cursor.saturating_sub(1);
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Right | Key::Ctrl('f') => {
                self.cursor = self.buf.len().min(self.cursor + 1);
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Home | Key::Ctrl('a') => {
                self.cursor = 0;
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::End | Key::Ctrl('e') => {
                self.cursor = self.buf.len();
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Up | Key::Ctrl('p') => {
                self.history_prev();
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Down | Key::Ctrl('n') => {
                self.history_next();
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Ctrl('u') => {
                self.buf.drain(..self.cursor);
                self.cursor = 0;
                self.redraw(out, prompt, prev_lines);
                LineEvent::Continue
            }
            Key::Ctrl('k') => {
                self.buf.truncate(self.cursor);
                self.redraw(out, prompt, prev_lines);
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
                    self.redraw(out, prompt, prev_lines);
                }
                LineEvent::Continue
            }
            Key::Ctrl('c') => LineEvent::Quit,
            Key::Char(c) => {
                self.buf.insert(self.cursor, c);
                self.cursor += 1;
                self.redraw(out, prompt, prev_lines);
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

    fn redraw(&self, out: &mut impl Write, prompt: &str, prev_lines: &mut usize) {
        if *prev_lines > 1 {
            write!(out, "{}", termion::cursor::Up((*prev_lines - 1) as u16)).ok();
        }
        write!(out, "\r{}", clear::AfterCursor).ok();

        write!(out, "{}", prompt).ok();
        let content: String = self.buf.iter().collect();
        write!(out, "{}", content.replace('\n', "\r\n")).ok();

        *prev_lines = 1 + self.buf.iter().filter(|&&c| c == '\n').count();

        let chars_after = self.buf.len() - self.cursor;
        if chars_after > 0 {
            let suffix = &self.buf[self.cursor..];
            let newlines_in_suffix = suffix.iter().filter(|&&c| c == '\n').count();
            let cols_back = chars_after - newlines_in_suffix;
            if newlines_in_suffix > 0 {
                write!(out, "{}", termion::cursor::Up(newlines_in_suffix as u16)).ok();
            }
            if cols_back > 0 {
                write!(out, "{}", termion::cursor::Left(cols_back as u16)).ok();
            }
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
    client: &AgentClient,
) -> Result<String, String> {
    loop {
        let trees = client.list_trees()?;

        if !trees.is_empty() {
            write!(out, "\r\nYour trees:\r\n").ok();
            for (i, tree) in trees.iter().enumerate() {
                print_tree_meta(out, tree, i);
            }
            write!(out, "\r\n").ok();
            write!(out, "Select a tree (number), 'new', or 'q' to quit: ").ok();
            out.flush().ok();

            input_line.clear();
            let mut prev_lines = 1;
            let result = loop {
                match keys.next() {
                    Some(Ok(k)) => match input_line.handle_key(k, out, "", &mut prev_lines) {
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
                return create_tree_interactive(input_line, keys, out, client);
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
            return create_tree_interactive(input_line, keys, out, client);
        }
    }
}

fn create_tree_interactive(
    input_line: &mut InputLine,
    keys: &mut impl Iterator<Item = Result<Key, std::io::Error>>,
    out: &mut impl Write,
    client: &AgentClient,
) -> Result<String, String> {
    let mut input_text = |prompt: &str| -> String {
        write!(out, "{}", prompt).ok();
        out.flush().ok();
        input_line.clear();
        let mut prev_lines = 1;
        let result = loop {
            match keys.next() {
                Some(Ok(k)) => match input_line.handle_key(k, out, "", &mut prev_lines) {
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

    let meta = client.create_tree(
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
    server: &str,
    tree_id: &str,
    text: &str,
    out: &mut impl Write,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut session = crate::client::AgentSession::connect(server, tree_id)?;
    session.set_nonblocking(true)?;
    session.send_message(text)?;
    write!(
        out,
        "  {}────────────────────────{}\r\n",
        color::Fg(color::LightBlack),
        style::Reset
    )
    .ok();
    out.flush().ok();

    let mut state = RenderState::default();
    let mut cancel_signalled = false;

    loop {
        // 1. Drain WS events
        loop {
            match session.try_next_event() {
                TryEvent::Event(ev) => {
                    if matches!(&ev, ServerEvent::Entry(_)) {
                        continue;
                    }
                    let done = matches!(&ev, ServerEvent::Done { .. });
                    render_event(out, &ev, &mut state);
                    if done {
                        return Ok(());
                    }
                    continue;
                }
                TryEvent::Closed => return Ok(()),
                TryEvent::Err(e) => {
                    write!(out, "\r\nws error: {}\r\n", e).ok();
                    return Ok(());
                }
                TryEvent::WouldBlock => break,
            }
        }

        // 2. Check Ctrl-C from the outer signal handler
        if stop.load(Ordering::Relaxed) {
            write!(out, "\r\nInterrupted\r\n").ok();
            break;
        }

        // 3. Peek stdin for Esc or Ctrl-C
        if !cancel_signalled {
            if let Some(key) = poll_key() {
                if matches!(key, Key::Esc | Key::Ctrl('c')) {
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
                }
            }
        }

        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

// ── Prompt loop ──

/// Run the interactive TUI.
pub fn run_interactive(
    server: &str,
    initial_repo_path: Option<String>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut out = io::stdout()
        .into_raw_mode()
        .map_err(|e| format!("raw mode: {}", e))?;
    let mut keys = io::stdin().keys();

    write!(
        out,
        "{}Connected to server at {}{}\r\n",
        color::Fg(color::Green),
        server,
        style::Reset
    )
    .ok();
    print_help(&mut out);
    write!(out, "\r\n").ok();

    let client = AgentClient::new(server);
    let mut input_line = InputLine::new();
    let mut current_tree_id = if let Some(rp) = initial_repo_path {
        let meta = client
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
        select_or_create_tree(&mut input_line, &mut keys, &mut out, &client)?
    };
    let mut show_header = true;

    loop {
        if stop.load(Ordering::Relaxed) {
            write!(out, "\r\nInterrupted\r\n").ok();
            break;
        }
        if show_header {
            match client.get_tree(&current_tree_id) {
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

                    if let Ok(entries) = client.get_entries(&current_tree_id) {
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

        write!(out, "\r\n> ").ok();
        out.flush().ok();

        input_line.clear();
        let mut prev_lines = 1;
        let result = loop {
            match keys.next() {
                Some(Ok(k)) => match input_line.handle_key(k, &mut out, "> ", &mut prev_lines) {
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
            CliCommand::ListTrees => match client.list_trees() {
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
                match client.create_tree(
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
                match client.get_tree(&id) {
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
            CliCommand::Stop => match client.stop_agent(&current_tree_id) {
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
            CliCommand::Show => match client.get_tree(&current_tree_id) {
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
                match client.get_entries(&current_tree_id) {
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
                process_message(server, &current_tree_id, &text, &mut out, stop)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_done_stop_is_happy_path() {
        let mut buf = Vec::new();
        render_done(&mut buf, "stop");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('✓'), "stop should show ✓, got: {output}");
        assert!(
            output.contains("Done"),
            "stop should show Done, got: {output}"
        );
        assert!(
            !output.contains("Stopped"),
            "stop should not show Stopped, got: {output}"
        );
        assert!(
            !output.contains("Aborted"),
            "stop should not show Aborted, got: {output}"
        );
    }

    #[test]
    fn test_render_done_complete_is_happy_path() {
        let mut buf = Vec::new();
        render_done(&mut buf, "complete");
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains('✓'),
            "complete should show ✓, got: {output}"
        );
        assert!(
            output.contains("Done"),
            "complete should show Done, got: {output}"
        );
    }

    #[test]
    fn test_render_done_aborted_is_red() {
        let mut buf = Vec::new();
        render_done(&mut buf, "aborted");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('✖'), "aborted should show ✖, got: {output}");
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
        assert!(
            !output.contains("Aborted"),
            "cancelled should not show Aborted, got: {output}"
        );
    }

    #[test]
    fn test_render_done_length() {
        let mut buf = Vec::new();
        render_done(&mut buf, "length");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('⚠'), "length should show ⚠, got: {output}");
        assert!(
            output.contains("Stopped at length limit"),
            "length should show warning, got: {output}"
        );
    }

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
        let mut prev = 1;
        il.handle_key(Key::Backspace, &mut buf, "> ", &mut prev);
        assert!(il.buf.is_empty());
    }

    #[test]
    fn test_inputline_cursor_movement() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        let mut prev = 1;
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        il.handle_key(Key::Left, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Left, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Char('X'), &mut buf, "> ", &mut prev);
        let result: String = il.buf.iter().collect();
        assert_eq!(result, "helXlo");
    }

    #[test]
    fn test_inputline_history_cycle() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        let mut prev = 1;

        for c in "first".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ", &mut prev);
        assert!(matches!(result, LineEvent::Submit(s) if s == "first"));

        for c in "second".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ", &mut prev);
        assert!(matches!(result, LineEvent::Submit(s) if s == "second"));

        il.handle_key(Key::Up, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Up, &mut buf, "> ", &mut prev);
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "first");

        il.handle_key(Key::Down, &mut buf, "> ", &mut prev);
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "second");

        il.handle_key(Key::Down, &mut buf, "> ", &mut prev);
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "");
    }

    #[test]
    fn test_inputline_alt_enter_inserts_newline() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        let mut prev = 1;
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        il.handle_key(Key::Alt('\n'), &mut buf, "> ", &mut prev);
        for c in "world".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        let result = il.handle_key(Key::Char('\n'), &mut buf, "> ", &mut prev);
        assert!(matches!(result, LineEvent::Submit(s) if s == "hello\nworld"));
    }

    #[test]
    fn test_inputline_ctrl_u_kills_to_start() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        let mut prev = 1;
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        il.handle_key(Key::Left, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Left, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Ctrl('u'), &mut buf, "> ", &mut prev);
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "lo");
        assert_eq!(il.cursor, 0);
    }

    #[test]
    fn test_inputline_ctrl_k_kills_to_end() {
        let mut il = InputLine::new();
        let mut buf = Vec::new();
        let mut prev = 1;
        for c in "hello".chars() {
            il.handle_key(Key::Char(c), &mut buf, "> ", &mut prev);
        }
        il.handle_key(Key::Left, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Left, &mut buf, "> ", &mut prev);
        il.handle_key(Key::Ctrl('k'), &mut buf, "> ", &mut prev);
        let s: String = il.buf.iter().collect();
        assert_eq!(s, "hel");
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
}

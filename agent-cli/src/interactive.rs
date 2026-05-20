//! Interactive TUI loop for the agent CLI.
//!
//! Two-thread architecture:
//! - **SSE thread:** reads events from the server's SSE stream, pushes to an mpsc queue.
//! - **Main thread:** renders events from the queue events and polls stdin for user input.
//!
//! Ctrl-C is handled via termion raw mode: `Key::Ctrl('c')` is detected directly from key events.
//! The main input loop and tree-selection prompts all use character-by-character raw input.

use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use termion::{color, style};
use termion::event::Key;
use termion::input::TermRead;
use termion::raw::IntoRawMode;

use agent_core::types::{Entry, ServerEvent, TreeMeta};

use crate::client::AgentClient;

// ── Command parsing ──

enum CliCommand {
    Message(String),
    ListTrees,
    Create { title: String, repo_path: Option<String>, model: Option<String> },
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
            let repo_path = parts.get(2).map(|s| s.to_string()).filter(|s| !s.is_empty());
            let model = parts.get(3).map(|s| s.to_string()).filter(|s| !s.is_empty());
            CliCommand::Create { title, repo_path, model }
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
    write!(out, "{}{}⚠ {}{}\r\n", color::Fg(color::Yellow), style::Bold, text, style::Reset).ok();
}

fn print_error(out: &mut impl Write, text: &str) {
    write!(out, "{}{}✖ {}{}\r\n", color::Fg(color::Red), style::Bold, text, style::Reset).ok();
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
    write!(out, "  /create <title> [path] [model]  Create a new tree\r\n").ok();
    write!(out, "  /switch <id>                Switch to a different tree\r\n").ok();
    write!(out, "  /stop                       Stop the active agent\r\n").ok();
    write!(out, "  /show                       Show current tree info\r\n").ok();
    write!(out, "  /entries [n]                Show last N entries (default 10)\r\n").ok();
    write!(out, "  /help                       Show this help\r\n").ok();
    write!(out, "  /quit                       Exit\r\n").ok();
    write!(out, "  <any text>                  Send as message to the agent\r\n").ok();
}

fn print_tree_meta(out: &mut impl Write, meta: &TreeMeta, index: usize) {
    let status = if meta.leaf_id.is_some() { "active" } else { "empty" };
    let title = meta.title.as_deref().unwrap_or("untitled");
    let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    write!(out, "  [{}] {} — {} ({})\r\n", index + 1, short_id, title, status).ok();
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
                    write!(out, "{}──────────────────────{}\r\n",
                           color::Fg(color::Blue), style::Reset).ok();
                }
                in_turn = true;

                let t = match &message.content {
                    agent_core::types::MessageContent::Text(t) => t.clone(),
                    _ => "[content blocks]".into(),
                };
                write!(out, "\r\n").ok();
                write!(out, "{}●  {}User:{}  {}\r\n",
                       color::Fg(color::Green), style::Bold, style::Reset, t).ok();
            }

            _ => {
                if !in_turn {
                    write!(out, "{}·  ·  ·{}\r\n",
                           color::Fg(color::LightBlack), style::Reset).ok();
                    in_turn = true;
                }
                render_event(out, &ServerEvent::Entry(entry.clone()), &mut state);
            }
        }
    }

    if let Some(last) = entries.last() {
        if !matches!(last, Entry::SessionEnd { .. }) {
            if in_turn {
                write!(out, "{}──────────────────────{}\r\n",
                       color::Fg(color::Blue), style::Reset).ok();
            }
            render_done(out, "complete");
        }
    }
}

fn render_done(out: &mut impl Write, status: &str) {
    let _ = match status {
        // Provider's "stop" finish_reason = model decided it was done.
        // "complete" is reserved for synthetic completion paths.
        // Both are happy-path turn endings.
        "stop" | "complete" => {
            write!(out, "\r\n  {}✓{} Done\r\n", color::Fg(color::Green), style::Reset)
        }
        // Model hit the provider's max_tokens or our hard cap.
        "length" => write!(out, "\r\n  {}⚠{} Stopped at length limit\r\n",
                           color::Fg(color::Yellow), style::Reset),
        // Worker crashed or was killed mid-turn.
        "aborted" => write!(out, "\r\n  {}✖{} Aborted\r\n",
                            color::Fg(color::Red), style::Reset),
        // Unknown status — show it so we notice in testing.
        other => write!(out, "\r\n  {}■{} Done ({}){}\r\n",
                        color::Fg(color::Yellow), style::Reset, other, style::Reset),
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
                    if t.len() > 100 { format!("{}...", &t[..100]) } else { t.clone() }
                }
                agent_core::types::MessageContent::Blocks(b) => format!("[{} blocks]", b.len()),
            };
            write!(out, "  {} ({}): {}\r\n", entry.id(), role_str, snippet).ok();
        }
        Entry::BashExec { command, exit_code, .. } => {
            write!(out, "  {} bash: {} (exit: {})\r\n", entry.id(), command, exit_code).ok();
        }
        Entry::GoalSet { goal, .. } => { let _ = write!(out, "  {} 🎯 Goal: {}\r\n", entry.id(), goal); }
        Entry::ModelSet { model, .. } => { let _ = write!(out, "  {} 🤖 Model: {}\r\n", entry.id(), model); }
        Entry::SessionEnd { status, summary, .. } => {
            let s = summary.as_deref().unwrap_or("no summary");
            let _ = write!(out, "  {} 📝 Session end ({:?}): {}\r\n", entry.id(), status, s);
        }
        Entry::SessionStart { .. } => { let _ = write!(out, "  {} ▶ Session start\r\n", entry.id()); }
        Entry::Label { label, .. } => { let _ = write!(out, "  {} 🏷 Label: {}\r\n", entry.id(), label); }
    }
}

// ── Event rendering ──

#[derive(Default)]
struct RenderState {
    _rendered: HashSet<String>,
    assistant_header_shown: bool,
}

/// Normalize bare `\n` to `\r\n` for raw-mode terminal output.
fn normalize_for_raw(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn render_event(out: &mut impl Write, event: &ServerEvent, state: &mut RenderState) {
    match event {
        ServerEvent::TextChunk { content } => {
            if !state.assistant_header_shown {
                state.assistant_header_shown = true;
                write!(out, "\r\n").ok();
                write!(out, "{}  Assistant:{}\r\n", color::Fg(color::Cyan), style::Reset).ok();
            }
            // Raw mode: `\n` alone leaves the cursor at the same column.
            // Normalize existing `\r\n` first (so we don't write `\r\r\n`), then
            // translate bare `\n` to `\r\n`.
            write!(out, "{}", normalize_for_raw(content)).ok();
            out.flush().ok();
        }
        ServerEvent::ToolStart { tool, input } => {
            write!(out, "\r\n").ok();
            let args_str = serde_json::to_string(input).unwrap_or_default();
            let preview = if args_str.len() > 120 {
                format!("{}...", &args_str[..120])
            } else {
                args_str
            };
            write!(out, "🛠  {}{}: {}{}\r\n", style::Bold, tool, preview, style::Reset).ok();
        }
        ServerEvent::ToolResult { tool, exit, output } => {
            write!(out, "\r\n").ok();
            let c = if *exit == 0 { color::Fg(color::Green).to_string() }
                     else { color::Fg(color::Red).to_string() };
            write!(out, "{}  {} (exit: {}){}\r\n", c, tool, exit, style::Reset).ok();
            if !output.is_empty() {
                print_indented(out, output, "│");
            }
        }
        ServerEvent::Entry(entry) => {
            match entry {
                Entry::Message { message, .. } if message.role == agent_core::types::MessageRole::User => {
                    let t = match &message.content {
                        agent_core::types::MessageContent::Text(t) => t.clone(),
                        _ => "[content blocks]".into(),
                    };
                    write!(out, "\r\n").ok();
                    write!(out, "{}●  {}User:{}  {}\r\n",
                           color::Fg(color::Green), style::Bold, style::Reset, t).ok();
                }
                Entry::GoalSet { goal, .. } => { write!(out, "\r\n").ok(); write!(out, "🎯  {}\r\n", goal).ok(); }
                Entry::ModelSet { model, .. } => { write!(out, "\r\n").ok(); write!(out, "🤖  Model: {}\r\n", model).ok(); }
                Entry::SessionEnd { summary, status, .. } => {
                    write!(out, "\r\n").ok();
                    let s = summary.as_deref().unwrap_or("");
                    write!(out, "📝 {}Session ended ({:?}){}{}\r\n",
                           style::Bold, status,
                           if s.is_empty() { String::new() } else { format!(": {}", s) },
                           style::Reset).ok();
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
                            write!(out, "{}  {}:{}\r\n", color::Fg(color::Cyan), role_label, style::Reset).ok();
                        }
                        write!(out, "{}\r\n", t).ok();
                    }
                }
                Entry::BashExec { command, output, exit_code, .. } => {
                    write!(out, "\r\n").ok();
                    write!(out, "{}  🛠  {}bash: {}{}\r\n",
                           color::Fg(color::Yellow), style::Bold, command, style::Reset).ok();
                    let c = if *exit_code == 0 { color::Fg(color::Green).to_string() }
                             else { color::Fg(color::Red).to_string() };
                    write!(out, "{}  bash (exit: {}){}\r\n", c, exit_code, style::Reset).ok();
                    if !output.is_empty() {
                        print_indented(out, output, "│");
                    }
                }
                _ => {}
            }
        }
        ServerEvent::CapWarning { level, pct } => {
            print_warning(out, &format!("Context at {}% ({})", pct, level));
        }
        ServerEvent::Error { message, fatal } => {
            if *fatal { print_error(out, &format!("Fatal: {}", message)); }
            else { print_warning(out, &format!("Error: {}", message)); }
        }
        ServerEvent::Done { status } => render_done(out, status),
        ServerEvent::FileChanged { path, kind } => {
            write!(out, "\r\n").ok(); write!(out, "  📄 {} ({})\r\n", path, kind).ok();
        }
        ServerEvent::MetaUpdate { title } => {
            if let Some(t) = title {
                write!(out, "\r\n").ok();
                write!(out, "  {}Title: {}{}\r\n", style::Bold, t, style::Reset).ok();
            }
        }
    }
}

// ── Raw-mode input helper ──

/// Read one line of input in raw mode, echoing characters back.
/// Returns `None` on Ctrl-C (user wants to quit).
fn read_line_raw(
    keys: &mut impl Iterator<Item = Result<Key, std::io::Error>>,
    out: &mut impl Write,
) -> Option<String> {
    let mut input = String::new();
    loop {
        match keys.next() {
            Some(Ok(Key::Ctrl('c'))) => {
                // Ctrl-C: signal quit
                write!(out, "\r\n").ok();
                out.flush().ok();
                return None;
            }
            Some(Ok(Key::Char('\n'))) | Some(Ok(Key::Char('\r'))) => {
                write!(out, "\r\n").ok();
                out.flush().ok();
                return Some(input);
            }
            Some(Ok(Key::Char(c))) => {
                input.push(c);
                write!(out, "{}", c).ok();
                out.flush().ok();
            }
            Some(Ok(Key::Backspace)) => {
                input.pop();
                write!(out, "\x08 \x08").ok();
                out.flush().ok();
            }
            Some(Err(e)) => {
                write!(out, "\r\nInput error: {}\r\n", e).ok();
                return Some(input);
            }
            None => return Some(input),
            _ => {}
        }
    }
}

// ── Tree selection ──

fn select_or_create_tree(
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

            let input = read_line_raw(keys, out);
            let input = match input {
                None => std::process::exit(0),
                Some(s) => s.trim().to_lowercase(),
            };

            if input == "q" || input == "quit" { std::process::exit(0); }
            if input == "new" { return create_tree_interactive(keys, out, client); }

            if let Ok(idx) = input.parse::<usize>() {
                if idx > 0 && idx <= trees.len() {
                    return Ok(trees[idx - 1].id.clone());
                }
            }

            if !input.is_empty() {
                let matches: Vec<&TreeMeta> = trees.iter().filter(|t| t.id.starts_with(&input)).collect();
                if matches.len() == 1 { return Ok(matches[0].id.clone()); }
                if matches.len() > 1 { write!(out, "Multiple matches, be more specific.\r\n").ok(); continue; }
            }

            write!(out, "Invalid selection.\r\n").ok();
        } else {
            write!(out, "No trees found. Let's create one.\r\n").ok();
            return create_tree_interactive(keys, out, client);
        }
    }
}

fn create_tree_interactive(
    keys: &mut impl Iterator<Item = Result<Key, std::io::Error>>,
    out: &mut impl Write,
    client: &AgentClient,
) -> Result<String, String> {
    write!(out, "Enter a title (or press Enter for 'default'): ").ok();
    out.flush().ok();
    let title = read_line_raw(keys, out).unwrap_or_default();
    let title = title.trim().to_string();
    let title = if title.is_empty() { "default".into() } else { title };

    write!(out, "Enter repo path (optional): ").ok();
    out.flush().ok();
    let repo_path = read_line_raw(keys, out).unwrap_or_default();
    let repo_path = repo_path.trim().to_string();
    let repo_path = if repo_path.is_empty() { None } else { Some(repo_path) };

    write!(out, "Enter model (optional): ").ok();
    out.flush().ok();
    let model = read_line_raw(keys, out).unwrap_or_default();
    let model = model.trim().to_string();
    let model = if model.is_empty() { None } else { Some(model) };

    let meta = client.create_tree(Some(&title), repo_path.as_deref(), model.as_deref(), &[], None, &[], &[])?;
    let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    write!(out, "{}Created tree {} ({}){}\r\n",
           color::Fg(color::Green), short_id,
           meta.title.as_deref().unwrap_or("untitled"),
           style::Reset).ok();
    Ok(meta.id)
}

// ── Message processing ──

fn process_message(
    server: &str,
    tree_id: &str,
    text: &str,
    out: &mut impl Write,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut session = crate::client::AgentSession::connect(server, tree_id)?;
    session.send_message(text)?;
    let mut state = RenderState::default();

    loop {
        if stop.load(Ordering::Relaxed) {
            write!(out, "\r\nInterrupted\r\n").ok();
            break;
        }
        match session.next_event() {
            Some(Ok(event)) => {
                let is_done = matches!(&event, ServerEvent::Done { .. });
                render_event(out, &event, &mut state);
                if is_done { break; }
            }
            Some(Err(e)) => {
                write!(out, "\r\nParse error: {}\r\n", e).ok();
                break;
            }
            None => {
                // Connection closed — check stop and loop back
            }
        }
    }
    Ok(())
}

// ── Prompt loop ──

/// Run the interactive TUI.
pub fn run_interactive(server: &str, initial_repo_path: Option<String>, stop: &AtomicBool) -> Result<(), String> {
    let mut out = io::stdout().into_raw_mode().map_err(|e| format!("raw mode: {}", e))?;
    let mut keys = io::stdin().keys();

    write!(out, "{}Connected to server at {}{}\r\n",
           color::Fg(color::Green), server, style::Reset).ok();
    print_help(&mut out);
    write!(out, "\r\n").ok();

    let client = AgentClient::new(server);
    let mut current_tree_id = if let Some(rp) = initial_repo_path {
        let meta = client.create_tree(Some("untitled"), Some(&rp), None, &[], None, &[], &[])
            .map_err(|e| format!("failed to create tree: {}", e))?;
        let sid = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
        write!(out, "Created tree {} in {}\r\n", sid, rp).ok();
        meta.id
    } else {
        select_or_create_tree(&mut keys, &mut out, &client)?
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
                    write!(out, "\r\n{}──────────────────────────────{}\r\n",
                           color::Fg(color::Blue), style::Reset).ok();
                    write!(out, "Now talking in: {} ({})\r\n", title, short_id).ok();
                    write!(out, "{}──────────────────────────────{}\r\n",
                           color::Fg(color::Blue), style::Reset).ok();

                    if let Ok(entries) = client.get_entries(&current_tree_id) {
                        if !entries.is_empty() {
                            let last: Vec<_> = entries.iter().rev().take(10).rev().cloned().collect();
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

        let input = match read_line_raw(&mut keys, &mut out) {
            None => { write!(out, "Goodbye!\r\n").ok(); break; }
            Some(s) => s,
        };
        let input = input.trim().to_string();
        if input.is_empty() { continue; }

        match parse_input(&input) {
            CliCommand::Quit => { write!(out, "Goodbye!\r\n").ok(); break; }
            CliCommand::Help => print_help(&mut out),
            CliCommand::ListTrees => {
                match client.list_trees() {
                    Ok(trees) => { write!(out, "\r\n").ok(); for (i, t) in trees.iter().enumerate() { print_tree_meta(&mut out, t, i); } }
                    Err(e) => print_error(&mut out, &format!("Failed to list trees: {}", e)),
                }
            }
            CliCommand::Create { title, repo_path, model } => {
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
                        let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
                        write!(out, "{}Created tree {} ({}){}\r\n",
                               color::Fg(color::Green), short_id,
                               meta.title.as_deref().unwrap_or("untitled"),
                               style::Reset).ok();
                    }
                    Err(e) => print_error(&mut out, &format!("Failed to create tree: {}", e)),
                }
            }
            CliCommand::Switch(id) => {
                if id.is_empty() { print_error(&mut out, "Usage: /switch <tree_id>"); continue; }
                match client.get_tree(&id) {
                    Ok(meta) => {
                        current_tree_id = meta.id;
                        show_header = true;
                        write!(out, "{}Switched to tree {}{}\r\n",
                               color::Fg(color::Green),
                               meta.title.as_deref().unwrap_or(&id),
                               style::Reset).ok();
                    }
                    Err(e) => print_error(&mut out, &format!("Tree not found: {}", e)),
                }
            }
            CliCommand::Stop => {
                match client.stop_agent(&current_tree_id) {
                    Ok(()) => { let _ = write!(out, "{}Stop signaled{}\r\n", color::Fg(color::Yellow), style::Reset); }
                    Err(e) => print_error(&mut out, &format!("Failed to stop: {}", e)),
                }
            }
            CliCommand::Show => {
                match client.get_tree(&current_tree_id) {
                    Ok(meta) => {
                        write!(out, "{}Tree info:{}\r\n", style::Bold, style::Reset).ok();
                        write!(out, "  ID:        {}\r\n", meta.id).ok();
                        write!(out, "  Title:     {}\r\n", meta.title.as_deref().unwrap_or("(none)")).ok();
                        write!(out, "  Repo path: {}\r\n", meta.repo_path.as_deref().map(|p| p.display().to_string()).unwrap_or("(none)".into())).ok();
                        write!(out, "  Active:    {}\r\n", if meta.leaf_id.is_some() { "yes" } else { "no" }).ok();
                    }
                    Err(e) => print_error(&mut out, &format!("Failed to load tree: {}", e)),
                }
            }
            CliCommand::Entries(n) => {
                let limit = n.unwrap_or(10);
                match client.get_entries(&current_tree_id) {
                    Ok(entries) => {
                        let last: Vec<_> = entries.iter().rev().take(limit).rev().collect();
                        write!(out, "{}Last {} entries:{}\r\n", style::Bold, last.len(), style::Reset).ok();
                        for e in &last { print_entry_summary(&mut out, e); }
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
        assert!(output.contains("Done"), "stop should show Done, got: {output}");
        assert!(!output.contains("Stopped"), "stop should not show Stopped, got: {output}");
        assert!(!output.contains("Aborted"), "stop should not show Aborted, got: {output}");
    }

    #[test]
    fn test_render_done_complete_is_happy_path() {
        let mut buf = Vec::new();
        render_done(&mut buf, "complete");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('✓'), "complete should show ✓, got: {output}");
        assert!(output.contains("Done"), "complete should show Done, got: {output}");
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
    fn test_render_done_length() {
        let mut buf = Vec::new();
        render_done(&mut buf, "length");
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('⚠'), "length should show ⚠, got: {output}");
        assert!(output.contains("Stopped at length limit"), "length should show warning, got: {output}");
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
}

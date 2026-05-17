//! Interactive TUI loop for the agent CLI.
//!
//! Two-thread architecture:
//! - **SSE thread:** reads events from the server's SSE stream, pushes to an mpsc queue.
//! - **Main thread:** renders events from the queue events and polls stdin for user input.

use std::collections::HashSet;
use std::io::{self, Write};


use termion::{color, style};

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

// ── Rendering helpers ──

fn print_warning(text: &str) {
    println!("{}{}⚠ {}{}", color::Fg(color::Yellow), style::Bold, text, style::Reset);
}

fn print_error(text: &str) {
    println!("{}{}✖ {}{}", color::Fg(color::Red), style::Bold, text, style::Reset);
}

fn print_indented(text: &str, prefix: &str) {
    for line in text.lines() {
        println!("  {} {}", prefix, line);
    }
}

fn print_help() {
    println!("{}Commands:{}", style::Bold, style::Reset);
    println!("  /trees                      List all trees");
    println!("  /create <title> [path] [model]  Create a new tree");
    println!("  /switch <id>                Switch to a different tree");
    println!("  /stop                       Stop the active agent");
    println!("  /show                       Show current tree info");
    println!("  /entries [n]                Show last N entries (default 10)");
    println!("  /help                       Show this help");
    println!("  /quit                       Exit");
    println!("  <any text>                  Send as message to the agent");
}

fn print_tree_meta(meta: &TreeMeta, index: usize) {
    let status = if meta.leaf_id.is_some() { "active" } else { "empty" };
    let title = meta.title.as_deref().unwrap_or("untitled");
    let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    println!("  [{}] {} — {} ({})", index + 1, short_id, title, status);
}

fn print_entry_summary(entry: &Entry) {
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
            println!("  {} ({}): {}", entry.id(), role_str, snippet);
        }
        Entry::BashExec { command, exit_code, .. } => {
            println!("  {} bash: {} (exit: {})", entry.id(), command, exit_code);
        }
        Entry::GoalSet { goal, .. } => println!("  {} 🎯 Goal: {}", entry.id(), goal),
        Entry::ModelSet { model, .. } => println!("  {} 🤖 Model: {}", entry.id(), model),
        Entry::SessionEnd { status, summary, .. } => {
            let s = summary.as_deref().unwrap_or("no summary");
            println!("  {} 📝 Session end ({:?}): {}", entry.id(), status, s);
        }
        Entry::SessionStart { .. } => println!("  {} ▶ Session start", entry.id()),
        Entry::Label { label, .. } => println!("  {} 🏷 Label: {}", entry.id(), label),
    }
}

// ── Event rendering ──

#[derive(Default)]
#[allow(dead_code)]
struct DedupTracker {
    rendered: HashSet<String>,
}

fn render_event(event: &ServerEvent, _dedup: &mut DedupTracker) {
    match event {
        ServerEvent::TextChunk { content } => {
            print!("{}", content);
            io::stdout().flush().ok();
        }
        ServerEvent::ToolStart { tool, input } => {
            println!();
            let args_str = serde_json::to_string(input).unwrap_or_default();
            let preview = if args_str.len() > 120 {
                format!("{}...", &args_str[..120])
            } else {
                args_str
            };
            println!("🛠  {}{}: {}{}", style::Bold, tool, preview, style::Reset);
        }
        ServerEvent::ToolResult { tool, exit, output } => {
            println!();
            let c = if *exit == 0 { color::Fg(color::Green).to_string() }
                     else { color::Fg(color::Red).to_string() };
            println!("{}  {} (exit: {}){}", c, tool, exit, style::Reset);
            if !output.is_empty() {
                print_indented(output, "│");
            }
        }
        ServerEvent::Entry(entry) => {
            match entry {
                Entry::Message { message, .. } if message.role == agent_core::types::MessageRole::User => {
                    println!();
                    let t = match &message.content {
                        agent_core::types::MessageContent::Text(t) => t.clone(),
                        _ => "[content blocks]".into(),
                    };
                    println!("●  {}", t);
                }
                Entry::GoalSet { goal, .. } => { println!(); println!("🎯  {}", goal); }
                Entry::ModelSet { model, .. } => { println!(); println!("🤖  Model: {}", model); }
                Entry::SessionEnd { summary, status, .. } => {
                    println!();
                    let s = summary.as_deref().unwrap_or("");
                    println!("📝 {}Session ended ({:?}){}{}",
                             style::Bold, status,
                             if s.is_empty() { String::new() } else { format!(": {}", s) },
                             style::Reset);
                }
                _ => {} // skip Message(assistant), BashExec, etc.
            }
        }
        ServerEvent::CapWarning { level, pct } => {
            print_warning(&format!("Context at {}% ({})", pct, level));
        }
        ServerEvent::Error { message, fatal } => {
            if *fatal { print_error(&format!("Fatal: {}", message)); }
            else { print_warning(&format!("Error: {}", message)); }
        }
        ServerEvent::Done { status } => {
            println!();
            match status.as_str() {
                "complete" => println!("  {}✓{} Done", color::Fg(color::Green), style::Reset),
                "stop" => println!("  {}■{} Stopped", color::Fg(color::Yellow), style::Reset),
                _ => println!("  {}{}", style::Bold, status),
            }
        }
        ServerEvent::FileChanged { path, kind } => {
            println!(); println!("  📄 {} ({})", path, kind);
        }
    }
}

// ── Tree selection ──

fn select_or_create_tree(client: &AgentClient) -> Result<String, String> {
    loop {
        let trees = client.list_trees()?;

        if !trees.is_empty() {
            println!("\nYour trees:");
            for (i, tree) in trees.iter().enumerate() {
                print_tree_meta(tree, i);
            }
            println!();
            print!("Select a tree (number), 'new', or 'q' to quit: ");
            io::stdout().flush().ok();

            let mut input = String::new();
            io::stdin().read_line(&mut input).ok();
            let input = input.trim().to_lowercase();

            if input == "q" || input == "quit" { std::process::exit(0); }
            if input == "new" { return create_tree_interactive(client); }

            if let Ok(idx) = input.parse::<usize>() {
                if idx > 0 && idx <= trees.len() {
                    return Ok(trees[idx - 1].id.clone());
                }
            }

            // Try as tree ID prefix
            if !input.is_empty() {
                let matches: Vec<&TreeMeta> = trees.iter().filter(|t| t.id.starts_with(&input)).collect();
                if matches.len() == 1 { return Ok(matches[0].id.clone()); }
                if matches.len() > 1 { println!("Multiple matches, be more specific."); continue; }
            }

            println!("Invalid selection.");
        } else {
            println!("No trees found. Let's create one.");
            return create_tree_interactive(client);
        }
    }
}

fn create_tree_interactive(client: &AgentClient) -> Result<String, String> {
    print!("Enter a title (or press Enter for 'default'): ");
    io::stdout().flush().ok();
    let mut title = String::new();
    io::stdin().read_line(&mut title).ok();
    let title = title.trim().to_string();
    let title = if title.is_empty() { "default".into() } else { title };

    print!("Enter repo path (optional): ");
    io::stdout().flush().ok();
    let mut repo_path = String::new();
    io::stdin().read_line(&mut repo_path).ok();
    let repo_path = repo_path.trim().to_string();
    let repo_path = if repo_path.is_empty() { None } else { Some(repo_path) };

    print!("Enter model (optional): ");
    io::stdout().flush().ok();
    let mut model = String::new();
    io::stdin().read_line(&mut model).ok();
    let model = model.trim().to_string();
    let model = if model.is_empty() { None } else { Some(model) };

    let meta = client.create_tree(Some(&title), repo_path.as_deref(), model.as_deref())?;
    let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    println!("{}Created tree {} ({}){}",
             color::Fg(color::Green), short_id,
             meta.title.as_deref().unwrap_or("untitled"),
             style::Reset);
    Ok(meta.id)
}

// ── Message processing ──

fn process_message(server: &str, tree_id: &str, text: &str) -> Result<(), String> {
    let client = AgentClient::new(server);
    println!("\n●  {}", text);
    client.send_message(tree_id, text)?;

    let mut stream = client.stream_events(tree_id)?;
    let mut dedup = DedupTracker::default();

    loop {
        match stream.next_event() {
            Some(event) => {
                let is_done = matches!(&event, ServerEvent::Done { .. });
                render_event(&event, &mut dedup);
                if is_done { break; }
            }
            None => break,
        }
    }
    Ok(())
}

// ── Prompt loop ──

/// Run the interactive TUI.
pub fn run_interactive(server: &str) -> Result<(), String> {
    println!("{}Connected to server at {}{}",
             color::Fg(color::Green), server, style::Reset);
    print_help();
    println!();

    let client = AgentClient::new(server);
    let mut current_tree_id = select_or_create_tree(&client)?;

    loop {
        // Show tree context
        match client.get_tree(&current_tree_id) {
            Ok(meta) => {
                let title = meta.title.as_deref().unwrap_or("untitled");
                let short_id = if current_tree_id.len() > 8 { &current_tree_id[..8] } else { &current_tree_id };
                println!("\n{}──────────────────────────────{}",
                         color::Fg(color::Blue), style::Reset);
                println!("Now talking in: {} ({})", title, short_id);
                println!("{}──────────────────────────────{}",
                         color::Fg(color::Blue), style::Reset);
            }
            Err(e) => print_warning(&format!("Failed to load tree: {}", e)),
        }

        // Prompt
        print!("\n> ");
        io::stdout().flush().ok();

        let mut input = String::new();
        io::stdin().read_line(&mut input).ok();
        let input = input.trim().to_string();
        if input.is_empty() { continue; }

        match parse_input(&input) {
            CliCommand::Quit => { println!("Goodbye!"); break; }
            CliCommand::Help => print_help(),
            CliCommand::ListTrees => {
                match client.list_trees() {
                    Ok(trees) => { println!(); for (i, t) in trees.iter().enumerate() { print_tree_meta(t, i); } }
                    Err(e) => print_error(&format!("Failed to list trees: {}", e)),
                }
            }
            CliCommand::Create { title, repo_path, model } => {
                match client.create_tree(
                    if title.is_empty() { None } else { Some(&title) },
                    repo_path.as_deref(),
                    model.as_deref(),
                ) {
                    Ok(meta) => {
                        current_tree_id = meta.id.clone();
                        let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
                        println!("{}Created tree {} ({}){}",
                                 color::Fg(color::Green), short_id,
                                 meta.title.as_deref().unwrap_or("untitled"),
                                 style::Reset);
                    }
                    Err(e) => print_error(&format!("Failed to create tree: {}", e)),
                }
            }
            CliCommand::Switch(id) => {
                if id.is_empty() { print_error("Usage: /switch <tree_id>"); continue; }
                match client.get_tree(&id) {
                    Ok(meta) => {
                        current_tree_id = meta.id;
                        println!("{}Switched to tree {}{}",
                                 color::Fg(color::Green),
                                 meta.title.as_deref().unwrap_or(&id),
                                 style::Reset);
                    }
                    Err(e) => print_error(&format!("Tree not found: {}", e)),
                }
            }
            CliCommand::Stop => {
                match client.stop_agent(&current_tree_id) {
                    Ok(()) => println!("{}Stop signaled{}", color::Fg(color::Yellow), style::Reset),
                    Err(e) => print_error(&format!("Failed to stop: {}", e)),
                }
            }
            CliCommand::Show => {
                match client.get_tree(&current_tree_id) {
                    Ok(meta) => {
                        println!("{}Tree info:{}", style::Bold, style::Reset);
                        println!("  ID:        {}", meta.id);
                        println!("  Title:     {}", meta.title.as_deref().unwrap_or("(none)"));
                        println!("  Repo path: {}", meta.repo_path.as_deref().map(|p| p.display().to_string()).unwrap_or("(none)".into()));
                        println!("  Active:    {}", if meta.leaf_id.is_some() { "yes" } else { "no" });
                    }
                    Err(e) => print_error(&format!("Failed to load tree: {}", e)),
                }
            }
            CliCommand::Entries(n) => {
                let limit = n.unwrap_or(10);
                match client.get_entries(&current_tree_id) {
                    Ok(entries) => {
                        let last: Vec<_> = entries.iter().rev().take(limit).rev().collect();
                        println!("{}Last {} entries:{}", style::Bold, last.len(), style::Reset);
                        for e in &last { print_entry_summary(e); }
                    }
                    Err(e) => print_error(&format!("Failed to load entries: {}", e)),
                }
            }
            CliCommand::Message(text) => {
                process_message(server, &current_tree_id, &text)?;
            }
        }
    }

    Ok(())
}
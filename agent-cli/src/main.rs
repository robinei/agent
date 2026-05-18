//! Agent CLI — command-line interface for the agent-server.
//!
//! ## Subcommands
//!
//! - `serve` — Start the server daemon (delegates to `agent-server`)
//! - `trees` — List all trees
//! - `create <title>` — Create a new tree
//! - `msg <tree_id> <message>` — One-shot: send message, display stream, exit
//! - `stop <tree_id>` — Stop an active agent
//! - (no command) — Interactive TUI mode
//!
//! All commands connect to a running agent-server at `--server` (default
//! `http://localhost:8080`).

use std::io::{self, Write};


use clap::Parser;

mod client;
mod interactive;

use std::process::Command;

/// Agent CLI — talk to coding agents via a local server.
#[derive(Parser)]
#[command(name = "agent-cli", version = "0.1.0")]
struct Cli {
    /// Server address (e.g., "localhost:8080" or "http://192.168.1.5:8080")
    #[arg(long, short = 's', default_value = "localhost:8080")]
    server: String,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Start the server daemon
    Serve {
        /// Path to config file
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    /// List all trees
    Trees,
    /// Create a new tree
    Create {
        /// Tree title
        title: String,
        /// Optional repo path
        #[arg(long)]
        repo_path: Option<String>,
        /// Optional model
        #[arg(long)]
        model: Option<String>,
    },
    /// Send a message and stream the response (one-shot)
    Msg {
        /// Tree ID
        tree_id: String,
        /// Message text
        message: String,
    },
    /// Stop an active agent
    Stop {
        /// Tree ID
        tree_id: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Some(SubCommand::Serve { config }) => {
            start_server(config.as_deref());
        }
        Some(SubCommand::Trees) => {
            list_trees(&cli.server);
        }
        Some(SubCommand::Create { title, repo_path, model }) => {
            create_tree(&cli.server, title, repo_path.as_deref(), model.as_deref());
        }
        Some(SubCommand::Msg { tree_id, message }) => {
            send_and_stream(&cli.server, tree_id, message);
        }
        Some(SubCommand::Stop { tree_id }) => {
            stop_agent(&cli.server, tree_id);
        }
        None => {
            // Interactive mode (default)
            match interactive::run_interactive(&cli.server) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}Error: {}", termion::color::Fg(termion::color::Red), e);
                    std::process::exit(1);
                }
            }
        }
    }
}

// ── Command implementations ──

fn start_server(config_path: Option<&str>) {
    let mut cmd = Command::new(
        std::env::current_exe()
            .ok()
            .and_then(|p| {
                let _name = p.file_name()?.to_str()?;
                // If running as `agent-cli`, the server binary is likely next to it
                let dir = p.parent()?;
                // Try agent-server first, then fallback to cargo run
                let server_path = dir.join("agent-server");
                if server_path.exists() {
                    Some(server_path)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "agent-server".into()),
    );

    if let Some(cfg) = config_path {
        cmd.arg("--config");
        cmd.arg(cfg);
    }

    println!("Starting agent-server...");
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to start server: {}", e);
            eprintln!("Try running `cargo run -p agent-server` manually.");
            std::process::exit(1);
        }
    };

    let status = child.wait().expect("Failed to wait on server process");
    std::process::exit(status.code().unwrap_or(1));
}

fn list_trees(server: &str) {
    let client = client::AgentClient::new(server);
    match client.list_trees() {
        Ok(trees) => {
            if trees.is_empty() {
                println!("No trees found.");
                return;
            }
            println!("Trees ({}):", trees.len());
            for tree in &trees {
                let short_id = if tree.id.len() > 8 { &tree.id[..8] } else { &tree.id };
                let status = if tree.leaf_id.is_some() { "active" } else { "empty" };
                let title = tree.title.as_deref().unwrap_or("untitled");
                println!("  {} — {} ({})", short_id, title, status);
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn create_tree(server: &str, title: &str, repo_path: Option<&str>, model: Option<&str>) {
    let client = client::AgentClient::new(server);
    match client.create_tree(Some(title), repo_path, model) {
        Ok(meta) => {
            let short_id = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
            println!("Created tree {} ({})", short_id, meta.title.as_deref().unwrap_or("untitled"));
            println!("ID: {}", meta.id);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn send_and_stream(server: &str, tree_id: &str, message: &str) {
    let client = client::AgentClient::new(server);

    // Open SSE stream FIRST (this auto-spawns the agent in a waiting state).
    // Then send the message, so we're already listening when events arrive.
    let mut stream = match client.stream_events(tree_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    // Send message SECOND
    match client.send_message(tree_id, message) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }

    loop {
        match stream.next_event() {
            Some(event) => {
                let is_done = match &event {
                    agent_core::types::ServerEvent::Done { .. } => true,
                    _ => false,
                };
                // Use simple text rendering
                match &event {
                    agent_core::types::ServerEvent::TextChunk { content } => {
                        print!("{}", content);
                        io::stdout().flush().ok();
                    }
                    agent_core::types::ServerEvent::ToolStart { tool, input } => {
                        println!();
                        let args = serde_json::to_string(input).unwrap_or_default();
                        println!("🛠  {}: {}", tool, args);
                    }
                    agent_core::types::ServerEvent::ToolResult { tool, exit, output } => {
                        println!();
                        println!("  {} (exit: {})", tool, exit);
                        for line in output.lines() {
                            println!("  │ {}", line);
                        }
                    }
                    agent_core::types::ServerEvent::Entry(entry) => {
                        match entry {
                            agent_core::types::Entry::GoalSet { goal, .. } => {
                                println!(); println!("🎯  {}", goal);
                            }
                            agent_core::types::Entry::ModelSet { model, .. } => {
                                println!(); println!("🤖  Model: {}", model);
                            }
                            agent_core::types::Entry::SessionEnd { summary, status, .. } => {
                                println!();
                                println!("📝 Session ended ({:?})", status);
                                if let Some(s) = summary {
                                    println!("   {}", s);
                                }
                            }
                            _ => {}
                        }
                    }
                    agent_core::types::ServerEvent::CapWarning { level, pct } => {
                        println!("\n⚠ Context at {}% ({})", pct, level);
                    }
                    agent_core::types::ServerEvent::Error { message, fatal } => {
                        if *fatal {
                            eprintln!("\nFatal: {}", message);
                        } else {
                            eprintln!("\nError: {}", message);
                        }
                    }
                    agent_core::types::ServerEvent::Done { status } => {
                        println!();
                        match status.as_str() {
                            "complete" | "stop" => println!("✓ Done"),
                            s => println!("{}", s),
                        }
                    }
                    _ => {}
                }
                if is_done {
                    break;
                }
            }
            None => break,
        }
    }
}

fn stop_agent(server: &str, tree_id: &str) {
    let client = client::AgentClient::new(server);
    match client.stop_agent(tree_id) {
        Ok(()) => println!("Stop signaled for tree {}", tree_id),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
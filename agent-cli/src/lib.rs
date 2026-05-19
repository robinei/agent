use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::process::Command;

use clap::Parser;

mod client;
mod interactive;

const EXIT_ERR: i32 = 1;

/// Agent CLI — talk to coding agents via a local server.
#[derive(Parser)]
#[command(name = "agent-cli", version = "0.1.0")]
struct Cli {
    /// Server address (e.g., "localhost:8080" or "http://192.168.1.5:8080")
    #[arg(long, short = 's', default_value = "localhost:8080")]
    server: String,

    /// Repo path (opens interactive session in this directory)
    repo_path: Option<String>,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Start the server daemon
    Serve {
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    /// List all trees
    Trees,
    /// Create a new tree
    Create {
        /// Tree title
        title: String,
        #[arg(long)]
        repo_path: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Send a message to an existing tree and stream the response
    Msg {
        tree_id: String,
        message: String,
    },
    /// Stop an active agent
    Stop {
        tree_id: String,
    },
    /// Create a tree, send a message, and auto-title (one-shot)
    Session {
        repo_path: String,
        message: String,
    },
}

pub fn run(args: Vec<String>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc::set_handler(move || s.store(true, Ordering::Relaxed)).ok();

    let full_args: Vec<String> = std::iter::once("agent-cli".to_string())
        .chain(args)
        .collect();
    let cli = Cli::parse_from(&full_args);

    match &cli.command {
        Some(SubCommand::Serve { config }) => start_server(config.as_deref()),
        Some(SubCommand::Trees) => list_trees(&cli.server),
        Some(SubCommand::Create { title, repo_path, model }) =>
            create_tree(&cli.server, title, repo_path.as_deref(), model.as_deref()),
        Some(SubCommand::Msg { tree_id, message }) =>
            send_and_stream(&cli.server, tree_id, message, &stop),
        Some(SubCommand::Session { repo_path, message }) =>
            session_and_stream(&cli.server, repo_path, message, &stop),
        Some(SubCommand::Stop { tree_id }) => stop_agent(&cli.server, tree_id),
        None => match interactive::run_interactive(&cli.server, cli.repo_path.clone(), &stop) {
            Ok(()) => {}
            Err(e) => { eprintln!("Error: {}", e); std::process::exit(EXIT_ERR); }
        },
    }
}

fn client(server: &str) -> client::AgentClient {
    client::AgentClient::new(server)
}

fn exit_err(msg: &str) -> ! {
    eprintln!("Error: {}", msg);
    std::process::exit(EXIT_ERR);
}

fn start_server(config_path: Option<&str>) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| {
            let dir = p.parent()?;
            let sp = dir.join("agent-server");
            if sp.exists() { Some(sp) } else { None }
        })
        .unwrap_or_else(|| "agent-server".into());

    let mut cmd = Command::new(path);
    if let Some(cfg) = config_path {
        cmd.arg("--config");
        cmd.arg(cfg);
    }

    println!("Starting agent-server...");
    let status = cmd.spawn()
        .unwrap_or_else(|e| { exit_err(&format!("Failed to start server: {e}")); })
        .wait()
        .expect("Failed to wait on server process");
    std::process::exit(status.code().unwrap_or(EXIT_ERR));
}

fn list_trees(server: &str) {
    match client(server).list_trees() {
        Ok(trees) => {
            if trees.is_empty() { println!("No trees found."); return; }
            println!("Trees ({}):", trees.len());
            for t in &trees {
                let sid = if t.id.len() > 8 { &t.id[..8] } else { &t.id };
                let status = if t.leaf_id.is_some() { "active" } else { "empty" };
                println!("  {} — {} ({})", sid, t.title.as_deref().unwrap_or("untitled"), status);
            }
        }
        Err(e) => exit_err(&e),
    }
}

fn create_tree(server: &str, title: &str, repo_path: Option<&str>, model: Option<&str>) {
    match client(server).create_tree(Some(title), repo_path, model) {
        Ok(meta) => {
            let sid = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
            println!("Created tree {} ({})", sid, meta.title.as_deref().unwrap_or("untitled"));
            println!("ID: {}", meta.id);
        }
        Err(e) => exit_err(&e),
    }
}

fn stop_agent(server: &str, tree_id: &str) {
    match client(server).stop_agent(tree_id) {
        Ok(()) => println!("Stop signaled for tree {}", tree_id),
        Err(e) => exit_err(&e),
    }
}

fn send_and_stream(server: &str, tree_id: &str, message: &str, stop: &AtomicBool) {
    let c = client(server);
    let mut stream = c.stream_events(tree_id).unwrap_or_else(|e| exit_err(&e));
    c.send_message(tree_id, message).unwrap_or_else(|e| exit_err(&e));
    stream_text_chunks(&mut stream, stop);
}

fn session_and_stream(server: &str, repo_path: &str, message: &str, stop: &AtomicBool) {
    let c = client(server);

    let abs = std::path::Path::new(repo_path);
    let abs = if abs.is_relative() {
        std::env::current_dir().unwrap_or_default().join(repo_path)
    } else {
        abs.to_path_buf()
    };
    let rp = abs.to_string_lossy().to_string();

    let meta = c.create_tree(Some("untitled"), Some(&rp), None)
        .unwrap_or_else(|e| exit_err(&e));
    let sid = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    println!("Created tree {} in {}", sid, rp);

    let mut stream = c.stream_events(&meta.id).unwrap_or_else(|e| exit_err(&e));
    c.send_message(&meta.id, message).unwrap_or_else(|e| exit_err(&e));
    stream_text_chunks(&mut stream, stop);

    match c.auto_title(&meta.id) {
        Ok(title) => println!("\nTitle: {}", title),
        Err(e) => eprintln!("Auto-title failed: {}", e),
    }
}

fn stream_text_chunks(stream: &mut client::SseEventStream, stop: &AtomicBool) {
    use agent_core::types::ServerEvent;
    loop {
        if stop.load(Ordering::Relaxed) {
            println!("\nInterrupted");
            break;
        }
        match stream.poll_event() {
            Some(ServerEvent::TextChunk { content }) => {
                print!("{content}");
                io::stdout().flush().ok();
            }
            Some(ServerEvent::Done { .. }) => { println!(); break; }
            Some(ServerEvent::Error { message, fatal }) => {
                if fatal { exit_err(&message); }
                else { eprintln!("Error: {message}"); }
            }
            Some(_) => {}
            None => {}
        }
    }
}
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::process::Command;
use std::os::unix::io::{FromRawFd, IntoRawFd};

use clap::Parser;

mod client;
mod interactive;
mod local;
pub mod markdown;
pub mod terminal;

use client::{AgentClient, AgentSession, ClientError};
use local::{LocalClient, LocalClientError};

const EXIT_ERR: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Local(#[from] LocalClientError),
    #[error("{0}")]
    Other(String),
}

impl From<String> for CliError {
    fn from(s: String) -> Self {
        CliError::Other(s)
    }
}

/// Agent CLI — talk to coding agents via a local server.
#[derive(Parser)]
#[command(name = "agent-cli", version = "0.1.0")]
struct Cli {
    #[arg(long, short = 's', default_value = "localhost:8080")]
    server: String,

    repo_path: Option<String>,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    Serve {
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    Trees,
    Create {
        title: String,
        #[arg(long)]
        repo_path: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, action = clap::ArgAction::Append)]
        writable: Vec<std::path::PathBuf>,
        #[arg(long, conflicts_with = "net")]
        no_net: bool,
        #[arg(long, conflicts_with = "no_net")]
        net: bool,
        #[arg(long, action = clap::ArgAction::Append)]
        hide: Vec<std::path::PathBuf>,
        #[arg(long, action = clap::ArgAction::Append)]
        unhide: Vec<std::path::PathBuf>,
    },
    Msg {
        tree_id: String,
        message: String,
    },
    Stop {
        tree_id: String,
    },
    Session {
        repo_path: String,
        message: String,
    },
}

enum Backend {
    Remote(AgentClient),
    Local(LocalClient),
}

impl Backend {
    fn list_trees(&self) -> Result<Vec<agent_core::types::TreeMeta>, CliError> {
        match self {
            Backend::Remote(c) => Ok(c.list_trees()?),
            Backend::Local(c) => Ok(c.list_trees()?),
        }
    }

    fn create_tree(
        &self,
        title: Option<&str>,
        repo_path: Option<&str>,
        model: Option<&str>,
        writable: &[std::path::PathBuf],
        network: Option<bool>,
        hide: &[std::path::PathBuf],
        unhide: &[std::path::PathBuf],
    ) -> Result<agent_core::types::TreeMeta, CliError> {
        match self {
            Backend::Remote(c) => Ok(c.create_tree(title, repo_path, model, writable, network, hide, unhide)?),
            Backend::Local(c) => Ok(c.create_tree(title, repo_path, model, writable, network, hide, unhide)?),
        }
    }

    fn get_tree(&self, id: &str) -> Result<agent_core::types::TreeMeta, CliError> {
        match self {
            Backend::Remote(c) => Ok(c.get_tree(id)?),
            Backend::Local(c) => Ok(c.get_tree(id)?),
        }
    }

    fn stop_agent(&self, tree_id: &str) -> Result<(), CliError> {
        match self {
            Backend::Remote(c) => Ok(c.stop_agent(tree_id)?),
            Backend::Local(c) => Ok(c.stop_agent(tree_id)?),
        }
    }

    fn connect_session(&self, tree_id: &str) -> Result<AgentSession, CliError> {
        match self {
            Backend::Remote(c) => {
                let (host, port) = client::parse_host_port(c.server_addr())?;
                Ok(AgentSession::connect(&format!("{}:{}", host, port), tree_id)?)
            }
            Backend::Local(c) => Ok(embedded_session(tree_id, c.config.clone())?),
        }
    }
}

// ── Backend resolution ──

fn resolve_backend(server: &str, explicit: bool) -> Backend {
    if explicit {
        return Backend::Remote(AgentClient::new(server));
    }
    // Fast TCP probe — succeeds if a server is already running.
    let addr = parse_server_addr(server).unwrap_or_else(|_| default_server_addr());
    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100)).is_ok() {
        eprintln!("Connected to server at {server}");
        return Backend::Remote(AgentClient::new(server));
    }
    // No server found — start embedded.
    eprintln!("No server at {server}, hosting server");
    let config = Arc::new(agent_core::config::load_config());
    agent_server::embed_init(config.clone(), false);
    // Start TCP listener in background so other clients can connect later.
    // Pass a never-signalled AtomicBool — the embedded server runs until the
    // CLI process exits; the CLI owns SIGINT via ctrlc and must not fight the
    // server's signal handler for it.
    let cc = config.clone();
    let no_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn(move || agent_server::serve(cc, no_shutdown));
    Backend::Local(LocalClient::new(config))
}

fn parse_server_addr(server: &str) -> Result<std::net::SocketAddr, CliError> {
    let s = server.strip_prefix("http://").or_else(|| server.strip_prefix("https://")).unwrap_or(server);
    let s = s.trim_end_matches('/');
    s.parse().map_err(|e| CliError::Other(format!("invalid server address '{}': {}", server, e)))
}

fn default_server_addr() -> std::net::SocketAddr {
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)), 8080)
}

// ── Socketpair session ──

pub fn embedded_session(
    tree_id: &str,
    config: Arc<agent_core::config::Config>,
) -> Result<AgentSession, CliError> {
    let (client_fd, server_fd) = nix::sys::socket::socketpair(
        nix::sys::socket::AddressFamily::Unix,
        nix::sys::socket::SockType::Stream,
        None,
        nix::sys::socket::SockFlag::empty(),
    )
    .map_err(|e| CliError::Other(e.to_string()))?;
    let server_stream = unsafe { TcpStream::from_raw_fd(server_fd.into_raw_fd()) };
    let client_stream = unsafe { TcpStream::from_raw_fd(client_fd.into_raw_fd()) };
    let cfg_for_server = config.clone();
    std::thread::spawn(move || {
        agent_server::http::handle_connection(server_stream, cfg_for_server);
    });
    Ok(AgentSession::from_stream(client_stream, tree_id)?)
}

use std::net::TcpStream;

// ── Entry point ──

pub fn run(args: Vec<String>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc::set_handler(move || s.store(true, Ordering::Relaxed)).ok();

    let full_args: Vec<String> = std::iter::once("agent-cli".to_string())
        .chain(args.clone())
        .collect();
    let cli = Cli::parse_from(&full_args);

    // Detect if --server was explicitly provided
    let explicit_server = args.iter().any(|a| a == "--server" || a == "-s");

    match &cli.command {
        Some(SubCommand::Serve { config }) => start_server(config.as_deref()),
        Some(SubCommand::Trees) => {
            let backend = resolve_backend(&cli.server, explicit_server);
            list_trees(&backend);
        }
        Some(SubCommand::Create { title, repo_path, model, writable, no_net, net, hide, unhide }) => {
            let backend = resolve_backend(&cli.server, explicit_server);
            create_tree(&backend, title, repo_path.as_deref(), model.as_deref(), writable, *no_net, *net, hide, unhide);
        }
        Some(SubCommand::Msg { tree_id, message }) => {
            let backend = resolve_backend(&cli.server, explicit_server);
            send_and_stream(&backend, tree_id, message, &stop);
        }
        Some(SubCommand::Session { repo_path, message }) => {
            let backend = resolve_backend(&cli.server, explicit_server);
            session_and_stream(&backend, repo_path, message, &stop);
        }
        Some(SubCommand::Stop { tree_id }) => {
            let backend = resolve_backend(&cli.server, explicit_server);
            stop_agent(&backend, tree_id);
        }
        None => {
            let backend = resolve_backend(&cli.server, explicit_server);
            match interactive::run_interactive(&backend, cli.repo_path.clone(), &stop) {
                Ok(()) => {}
                Err(e) => { eprintln!("Error: {}", e); std::process::exit(EXIT_ERR); }
            }
        },
    }
}

fn exit_err(msg: impl std::fmt::Display) -> ! {
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

fn list_trees(backend: &Backend) {
    match backend.list_trees() {
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

fn create_tree(
    backend: &Backend,
    title: &str,
    repo_path: Option<&str>,
    model: Option<&str>,
    writable: &[std::path::PathBuf],
    no_net: bool,
    net: bool,
    hide: &[std::path::PathBuf],
    unhide: &[std::path::PathBuf],
) {
    let network = if no_net { Some(false) } else if net { Some(true) } else { None };
    match backend.create_tree(Some(title), repo_path, model, writable, network, hide, unhide) {
        Ok(meta) => {
            let sid = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
            println!("Created tree {} ({})", sid, meta.title.as_deref().unwrap_or("untitled"));
            println!("ID: {}", meta.id);
        }
        Err(e) => exit_err(&e),
    }
}

fn stop_agent(backend: &Backend, tree_id: &str) {
    match backend.stop_agent(tree_id) {
        Ok(()) => println!("Stop signaled for tree {}", tree_id),
        Err(e) => exit_err(&e),
    }
}

fn send_and_stream(backend: &Backend, tree_id: &str, message: &str, stop: &AtomicBool) {
    use agent_core::types::{DiagnosticSeverity, NotificationLevel, ServerEvent};
    let mut session = backend.connect_session(tree_id).unwrap_or_else(|e| exit_err(&e));
    session.send_message(message).unwrap_or_else(|e| exit_err(&e));
    loop {
        if stop.load(Ordering::Relaxed) {
            println!("\nInterrupted");
            break;
        }
        match session.next_event() {
            Some(Ok(ServerEvent::TextChunk { content })) => {
                print!("{content}");
                io::stdout().flush().ok();
            }
            Some(Ok(ServerEvent::Done { .. })) => { println!(); break; }
            Some(Ok(ServerEvent::Notification { level, message })) => {
                if level == NotificationLevel::Fatal { exit_err(&message); }
                else { eprintln!("{message}"); }
            }
            Some(Ok(ServerEvent::Diagnostics { source, files })) => {
                for file in &files {
                    for diag in &file.diagnostics {
                        let sev = match diag.severity {
                            Some(DiagnosticSeverity::Error) => "error",
                            Some(DiagnosticSeverity::Warning) => "warning",
                            _ => "info",
                        };
                        eprintln!("[{}] {}:{}  {}: {}", source, file.path, diag.range.start.line + 1, sev, diag.message);
                    }
                }
            }
            Some(Err(e)) => { eprintln!("Parse error: {e}"); break; }
            _ => {}
        }
    }
}

fn session_and_stream(backend: &Backend, repo_path: &str, message: &str, stop: &AtomicBool) {
    use agent_core::types::{NotificationLevel, ServerEvent};

    let abs = std::path::Path::new(repo_path);
    let abs = if abs.is_relative() {
        std::env::current_dir().unwrap_or_default().join(repo_path)
    } else {
        abs.to_path_buf()
    };
    let rp = abs.to_string_lossy().to_string();

    let meta = backend.create_tree(
        None,
        Some(&rp),
        None,
        &[],
        None,
        &[],
        &[],
    )
    .unwrap_or_else(|e| exit_err(&e));
    let sid = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
    println!("Created tree {} in {}", sid, rp);

    let mut session = backend.connect_session(&meta.id).unwrap_or_else(|e| exit_err(&e));
    session.send_message(message).unwrap_or_else(|e| exit_err(&e));

    loop {
        if stop.load(Ordering::Relaxed) {
            println!("\nInterrupted");
            break;
        }
        match session.next_event() {
            Some(Ok(ServerEvent::TextChunk { content })) => {
                print!("{content}");
                io::stdout().flush().ok();
            }
            Some(Ok(ServerEvent::Done { .. })) => { println!(); break; }
            Some(Ok(ServerEvent::Notification { level, message })) => {
                if level == NotificationLevel::Fatal { exit_err(&message); }
                else { eprintln!("{message}"); }
            }
            Some(Ok(ServerEvent::Diagnostics { source, files })) => {
                use agent_core::types::DiagnosticSeverity;
                for file in &files {
                    for diag in &file.diagnostics {
                        let sev = match diag.severity {
                            Some(DiagnosticSeverity::Error) => "error",
                            Some(DiagnosticSeverity::Warning) => "warning",
                            _ => "info",
                        };
                        eprintln!("[{}] {}:{}  {}: {}", source, file.path, diag.range.start.line + 1, sev, diag.message);
                    }
                }
            }
            Some(Err(e)) => { eprintln!("Parse error: {e}"); break; }
            _ => {}
        }
    }
    
    // Listen for MetaUpdate emitted by the worker after auto-title completes.
    use client::TryEvent;
    session.set_nonblocking(true).ok();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if stop.load(Ordering::Relaxed) || std::time::Instant::now() >= deadline {
            break;
        }
        match session.try_next_event() {
            TryEvent::Event(ServerEvent::MetaUpdate { title: Some(t) }) => {
                println!("\nTitle: {}", t);
                break;
            }
            TryEvent::Event(_) => continue,
            TryEvent::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(100)),
            TryEvent::Closed | TryEvent::Err(_) => break,
        }
    }
}
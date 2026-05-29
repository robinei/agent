//! agent-cli library — async CLI for the agent-server.
//!
//! Wraps the CLI entry in a `current_thread` tokio runtime. All I/O uses
//! `reqwest` (HTTP) and `tokio-tungstenite` (WebSocket). Embedded mode
//! starts the server on a loopback TCP port and connects via the same
//! HTTP/WS path — no socketpair, no `local.rs`.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

mod app;
mod client;
mod interactive;
pub mod markdown;
mod tui;

use client::{AgentClient, AgentSession, ClientError};

const EXIT_ERR: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Client(#[from] ClientError),
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

/// Backend abstraction — always remote HTTP/WS. The embedded path starts a
/// server internally and then uses the same `AgentClient`.
struct Backend {
    client: AgentClient,
}

impl Backend {
    fn new(client: AgentClient) -> Self {
        Self { client }
    }

    async fn list_trees(&self) -> Result<Vec<agent_core::types::TreeMeta>, CliError> {
        Ok(self.client.list_trees().await?)
    }

    async fn create_tree(
        &self,
        title: Option<&str>,
        repo_path: Option<&str>,
        model: Option<&str>,
        writable: &[std::path::PathBuf],
        network: Option<bool>,
        hide: &[std::path::PathBuf],
        unhide: &[std::path::PathBuf],
    ) -> Result<agent_core::types::TreeMeta, CliError> {
        Ok(self
            .client
            .create_tree(title, repo_path, model, writable, network, hide, unhide)
            .await?)
    }

    async fn get_tree(&self, id: &str) -> Result<agent_core::types::TreeMeta, CliError> {
        Ok(self.client.get_tree(id).await?)
    }

    async fn stop_agent(&self, tree_id: &str) -> Result<(), CliError> {
        Ok(self.client.stop_agent(tree_id).await?)
    }

    async fn connect_session(&self, tree_id: &str) -> Result<AgentSession, CliError> {
        let url = self.client.ws_url_for(tree_id);
        Ok(AgentSession::connect(&url, "").await?)
    }
}

// ── Backend resolution ────────────────────────────────────────────────────

/// Resolve the backend: try connecting to an existing server; if none is
/// running, start an embedded in-process server on a loopback port.
async fn resolve_backend(server: &str, explicit: bool) -> Backend {
    if explicit {
        return Backend::new(AgentClient::new(server));
    }

    // Fast TCP probe — succeeds if a server is already running.
    let addr = parse_server_addr(server).unwrap_or_else(|_| default_server_addr());
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
        eprintln!("Connected to server at {server}");
        return Backend::new(AgentClient::new(server));
    }

    // No server found — start embedded on a random port.
    eprintln!("No server at {server}, starting embedded server...");
    let mut config = agent_core::config::load_config();

    // Disable stderr logging for the embedded server — the TUI owns the
    // terminal and log lines would corrupt the alternate-screen display.
    config.logging.to_stderr = false;
    let config = Arc::new(config);

    // Bind to a random loopback port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind embedded server listener");
    let port = listener.local_addr().unwrap().port();
    let server_addr = format!("127.0.0.1:{}", port);

    // Spawn the server on this listener.
    let cfg_for_server = config.clone();
    let _server_handle = tokio::spawn(async move {
        agent_server::serve_on(listener, cfg_for_server).await;
    });

    // Wait for server readiness with retries.
    let client = AgentClient::new(&server_addr);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client.list_trees().await {
            Ok(_) => {
                eprintln!("Embedded server ready on {server_addr}");
                break;
            }
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    eprintln!("Warning: embedded server not responding after 5s, continuing anyway");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    Backend::new(AgentClient::new(&server_addr))
}

fn parse_server_addr(server: &str) -> Result<std::net::SocketAddr, CliError> {
    let s = server
        .strip_prefix("http://")
        .or_else(|| server.strip_prefix("https://"))
        .unwrap_or(server);
    let s = s.trim_end_matches('/');
    s.parse()
        .map_err(|e| CliError::Other(format!("invalid server address '{}': {}", server, e)))
}

fn default_server_addr() -> std::net::SocketAddr {
    std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        8080,
    )
}

// ── Entry point ───────────────────────────────────────────────────────────

pub fn run(args: Vec<String>) {
    let stop = Arc::new(AtomicBool::new(false));

    let full_args: Vec<String> =
        std::iter::once("agent-cli".to_string())
            .chain(args.clone())
            .collect();
    let cli = Cli::parse_from(&full_args);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(async {
            // Signal handler: set stop on Ctrl+C.
            let s = stop.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                eprintln!("\nInterrupt signal received");
                s.store(true, Ordering::Relaxed);
            });

            let explicit_server = args.iter().any(|a| a == "--server" || a == "-s");

            match &cli.command {
                Some(SubCommand::Serve { config }) => {
                    start_server(config.as_deref()).await;
                }
                Some(SubCommand::Trees) => {
                    let backend = resolve_backend(&cli.server, explicit_server).await;
                    list_trees(&backend).await;
                }
                Some(SubCommand::Create {
                    title,
                    repo_path,
                    model,
                    writable,
                    no_net,
                    net,
                    hide,
                    unhide,
                }) => {
                    let backend = resolve_backend(&cli.server, explicit_server).await;
                    create_tree(
                        &backend,
                        title,
                        repo_path.as_deref(),
                        model.as_deref(),
                        writable,
                        *no_net,
                        *net,
                        hide,
                        unhide,
                    )
                    .await;
                }
                Some(SubCommand::Msg {
                    tree_id,
                    message,
                }) => {
                    let backend = resolve_backend(&cli.server, explicit_server).await;
                    send_and_stream(&backend, tree_id, message, &stop).await;
                }
                Some(SubCommand::Session {
                    repo_path,
                    message,
                }) => {
                    let backend = resolve_backend(&cli.server, explicit_server).await;
                    session_and_stream(&backend, repo_path, message, &stop).await;
                }
                Some(SubCommand::Stop { tree_id }) => {
                    let backend = resolve_backend(&cli.server, explicit_server).await;
                    stop_agent(&backend, tree_id).await;
                }
                None => {
                    let backend = resolve_backend(&cli.server, explicit_server).await;
                    match interactive::run_interactive(&backend, cli.repo_path.clone(), &stop).await
                    {
                        Ok(()) => {}
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            std::process::exit(EXIT_ERR);
                        }
                    }
                }
            }
        });
}

// ── Server subcommand ────────────────────────────────────────────────────

async fn start_server(config_path: Option<&str>) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| {
            let dir = p.parent()?;
            let sp = dir.join("agent-server");
            if sp.exists() {
                Some(sp)
            } else {
                None
            }
        })
        .unwrap_or_else(|| "agent-server".into());

    let mut cmd = tokio::process::Command::new(&path);
    if let Some(cfg) = config_path {
        cmd.arg("--config");
        cmd.arg(cfg);
    }

    println!("Starting agent-server...");
    let status = cmd
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("Failed to start server: {}", e);
            std::process::exit(EXIT_ERR);
        })
        .wait()
        .await
        .expect("Failed to wait on server process");
    std::process::exit(status.code().unwrap_or(EXIT_ERR));
}

// ── Command helpers ──────────────────────────────────────────────────────

fn exit_err(msg: impl std::fmt::Display) -> ! {
    eprintln!("Error: {}", msg);
    std::process::exit(EXIT_ERR);
}

async fn list_trees(backend: &Backend) {
    match backend.list_trees().await {
        Ok(trees) => {
            if trees.is_empty() {
                println!("No trees found.");
                return;
            }
            println!("Trees ({}):", trees.len());
            for t in &trees {
                let sid = if t.id.len() > 8 { &t.id[..8] } else { &t.id };
                let status = if t.leaf_id.is_some() {
                    "active"
                } else {
                    "empty"
                };
                println!(
                    "  {} — {} ({})",
                    sid,
                    t.title.as_deref().unwrap_or("untitled"),
                    status
                );
            }
        }
        Err(e) => exit_err(&e),
    }
}

async fn create_tree(
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
    let network = if no_net {
        Some(false)
    } else if net {
        Some(true)
    } else {
        None
    };
    match backend
        .create_tree(Some(title), repo_path, model, writable, network, hide, unhide)
        .await
    {
        Ok(meta) => {
            let sid = if meta.id.len() > 8 { &meta.id[..8] } else { &meta.id };
            println!(
                "Created tree {} ({})",
                sid,
                meta.title.as_deref().unwrap_or("untitled")
            );
            println!("ID: {}", meta.id);
        }
        Err(e) => exit_err(&e),
    }
}

async fn stop_agent(backend: &Backend, tree_id: &str) {
    match backend.stop_agent(tree_id).await {
        Ok(()) => println!("Stop signaled for tree {}", tree_id),
        Err(e) => exit_err(&e),
    }
}

// ── Streaming (oneshot Msg / Session commands) ──────────────────────────

/// Render an entry to plain text lines (no terminal styling).
pub fn render_entry_lines(entry: &agent_core::types::Entry) -> Vec<String> {
    use agent_core::types::*;
    let mut out = Vec::new();
    match entry {
        Entry::Message { message, .. } => {
            let content = match &message.content {
                MessageContent::Text(t) => t.clone(),
                _ => "[content blocks]".into(),
            };
            if let Some(ref thinking) = message.thinking {
                if !thinking.is_empty() {
                    out.push(thinking.clone());
                }
            }
            match message.role {
                MessageRole::User => out.push(format!("> {}", content)),
                _ => out.push(content.clone()),
            }
        }
        Entry::GoalSet { goal, .. } => out.push(format!("🎯  {}", goal)),
        Entry::ModelSet { model, .. } => out.push(format!("🤖  Model: {}", model)),
        Entry::SessionEnd {
            summary, status, ..
        } => {
            let s = summary.as_deref().unwrap_or("");
            if s.is_empty() {
                out.push(format!("📝 Session ended ({:?})", status));
            } else {
                out.push(format!("📝 Session ended ({:?}): {}", status, s));
            }
        }
        Entry::BashExec {
            command,
            output,
            exit_code,
            ..
        } => {
            out.push(format!("  🛠  bash: {}", command));
            out.push(format!("  bash (exit: {})", exit_code));
            if !output.is_empty() {
                for line in output.lines() {
                    out.push(format!("    │ {}", line));
                }
            }
        }
        _ => {}
    }
    out
}

async fn send_and_stream(backend: &Backend, tree_id: &str, message: &str, stop: &AtomicBool) {
    use agent_core::types::*;
    let mut session = backend
        .connect_session(tree_id)
        .await
        .unwrap_or_else(|e| exit_err(&e));
    session
        .send_message(message)
        .await
        .unwrap_or_else(|e| exit_err(&e));

    loop {
        if stop.load(Ordering::Relaxed) {
            println!("\nInterrupted");
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), session.next_event()).await {
            Ok(Some(Ok(ServerEvent::TextChunk { content }))) => {
                print!("{}", content);
                io::stdout().flush().ok();
            }
            Ok(Some(Ok(ServerEvent::Done { status, .. }))) if status != "history" => {
                println!();
                break;
            }
            Ok(Some(Ok(ServerEvent::Done { .. }))) => {} // history done, ignore
            Ok(Some(Ok(ServerEvent::Notification { level, message }))) => {
                if level == NotificationLevel::Fatal {
                    exit_err(&message);
                } else {
                    eprintln!("{}", message);
                }
            }
            Ok(Some(Ok(ServerEvent::Diagnostics { source, files }))) => {
                for file in &files {
                    for diag in &file.diagnostics {
                        let sev = match diag.severity {
                            Some(DiagnosticSeverity::Error) => "error",
                            Some(DiagnosticSeverity::Warning) => "warning",
                            _ => "info",
                        };
                        eprintln!(
                            "[{}] {}:{}  {}: {}",
                            source,
                            file.path,
                            diag.range.start.line + 1,
                            sev,
                            diag.message
                        );
                    }
                }
            }
            Ok(Some(Ok(ServerEvent::Entry(entry)))) => {
                for line in render_entry_lines(&entry) {
                    println!("{}", line);
                }
            }
            Ok(Some(Ok(_))) => {} // other events (ToolStart, ContextUpdate, etc.)
            Ok(Some(Err(e))) => {
                eprintln!("Parse error: {}", e);
                break;
            }
            Ok(None) => break,    // WebSocket closed
            Err(_) => {}          // timeout — check stop flag and loop
        }
    }
}

async fn session_and_stream(backend: &Backend, repo_path: &str, message: &str, stop: &AtomicBool) {
    use agent_core::types::*;

    let abs = std::path::Path::new(repo_path);
    let abs = if abs.is_relative() {
        std::env::current_dir().unwrap_or_default().join(repo_path)
    } else {
        abs.to_path_buf()
    };
    let rp = abs.to_string_lossy().to_string();

    let meta = backend
        .create_tree(None, Some(&rp), None, &[], None, &[], &[])
        .await
        .unwrap_or_else(|e| exit_err(&e));
    let sid = if meta.id.len() > 8 {
        &meta.id[..8]
    } else {
        &meta.id
    };
    println!("Created tree {} in {}", sid, rp);

    let mut session = backend
        .connect_session(&meta.id)
        .await
        .unwrap_or_else(|e| exit_err(&e));
    session
        .send_message(message)
        .await
        .unwrap_or_else(|e| exit_err(&e));

    loop {
        if stop.load(Ordering::Relaxed) {
            println!("\nInterrupted");
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), session.next_event()).await {
            Ok(Some(Ok(ServerEvent::TextChunk { content }))) => {
                print!("{}", content);
                io::stdout().flush().ok();
            }
            Ok(Some(Ok(ServerEvent::Done { status, .. }))) if status != "history" => {
                println!();
                break;
            }
            Ok(Some(Ok(ServerEvent::Done { .. }))) => {} // history done, ignore
            Ok(Some(Ok(ServerEvent::Notification { level, message }))) => {
                if level == NotificationLevel::Fatal {
                    exit_err(&message);
                } else {
                    eprintln!("{}", message);
                }
            }
            Ok(Some(Ok(ServerEvent::Diagnostics { source, files }))) => {
                for file in &files {
                    for diag in &file.diagnostics {
                        let sev = match diag.severity {
                            Some(DiagnosticSeverity::Error) => "error",
                            Some(DiagnosticSeverity::Warning) => "warning",
                            _ => "info",
                        };
                        eprintln!(
                            "[{}] {}:{}  {}: {}",
                            source,
                            file.path,
                            diag.range.start.line + 1,
                            sev,
                            diag.message
                        );
                    }
                }
            }
            Ok(Some(Ok(ServerEvent::Entry(entry)))) => {
                for line in render_entry_lines(&entry) {
                    println!("{}", line);
                }
            }
            Ok(Some(Ok(_))) => {} // other events
            Ok(Some(Err(e))) => {
                eprintln!("Parse error: {}", e);
                break;
            }
            Ok(None) => break,    // WebSocket closed
            Err(_) => {}          // timeout — check stop flag and loop
        }
    }

    // Listen for MetaUpdate emitted by the worker after auto-title completes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if stop.load(Ordering::Relaxed) || tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(100), session.next_event()).await {
            Ok(Some(Ok(ServerEvent::MetaUpdate {
                title: Some(t), ..
            }))) => {
                println!("\nTitle: {}", t);
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                eprintln!("Parse error: {}", e);
                break;
            }
            Ok(None) => break,
            Err(_) => {} // timeout, loop
        }
    }
}

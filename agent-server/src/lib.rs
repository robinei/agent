use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_core::config::Config;
use agent_core::store::Store;

pub mod auto_title;
pub mod handlers;
pub mod http;
pub mod lifecycle;
mod llm_handler;
pub mod provider;
mod routes;
pub mod worker_ctx;
pub mod worker_loop;
mod ws;
pub mod ws_client;

/// Initialise shared state: index rebuild, session recovery, startup hooks.
/// Does not bind any socket or register signal handlers.
/// Called by both `run()` and the CLI's embedded boot path.
pub fn embed_init(config: Arc<Config>, store: Arc<Store>, to_stderr: bool) {
    let log_file = config.logging.to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging.level, to_stderr);

    match store.rebuild_index() {
        Ok(trees) => log::info!("Rebuilt index: {} trees loaded", trees.len()),
        Err(e) => log::warn!("Index rebuild issue: {}", e),
    }

    // Scan for unterminated sessions from a previous unclean shutdown
    let unterm = store.scan_unterminated();
    for id in &unterm {
        log::info!(
            "[lifecycle] recovering unterminated session for tree {}",
            id
        );
        lifecycle::recover_tree(&store, id);
    }
    if !unterm.is_empty() {
        log::info!(
            "[lifecycle] cleaned up {} unterminated session(s)",
            unterm.len()
        );
    }

}

/// Bind the TCP listener and run the accept loop (blocks until shutdown signal).
/// Spawns one thread per connection, same as today.
/// `shutdown` is set by the caller (via signal handlers in `run()`, or simply
/// never set when called from the embedded CLI path).
pub fn serve(config: Arc<Config>, store: Arc<Store>, shutdown: Arc<AtomicBool>) {
    let bind = format!("{}:{}", config.server.host, config.server.port);
    let listener = std::net::TcpListener::bind(&bind).expect("bind");
    log::debug!("Listening on http://{}", bind);
    listener.set_nonblocking(true).ok();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            log::info!("[lifecycle] shutting down (signal received)");
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let store = store.clone();
                let cfg = config.clone();
                std::thread::spawn(move || http::handle_connection(stream, store, cfg));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                log::warn!("[serve] accept error: {e}");
                continue;
            }
        }
    }

    lifecycle::shutdown_all(&store);
    log::info!("[lifecycle] server stopped");
}

pub fn run(args: Vec<String>) {
    let _ = args;

    let config = Arc::new(agent_core::config::load_config());

    let log_file = config.logging.to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging.level, config.logging.to_stderr);

    log::info!("Starting agent-server...");
    log::info!(
        "Config: host={}, port={}, model={}",
        config.server.host,
        config.server.port,
        config.provider.model
    );

    let store = Arc::new(agent_core::store::Store::default());
    embed_init(config.clone(), store.clone(), config.logging.to_stderr);

    let shutdown = Arc::new(AtomicBool::new(false));
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone()) {
        log::warn!("Failed to register SIGINT handler: {}", e);
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone()) {
        log::warn!("Failed to register SIGTERM handler: {}", e);
    }
    serve(config, store, shutdown);
}

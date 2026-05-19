use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod http;
mod lifecycle;
mod routes;
mod ws;

pub fn run(args: Vec<String>) {
    let _ = args;

    let config = Arc::new(agent_core::config::load_config());

    let log_file = config
        .logging
        .to_file
        .as_ref()
        .and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging.level);

    log::info!("Starting agent-server...");
    log::info!(
        "Config: host={}, port={}, model={}",
        config.server.host,
        config.server.port,
        config.provider.model
    );

    let store = Arc::new(agent_core::store::Store::default());

    match store.rebuild_index() {
        Ok(trees) => log::info!("Rebuilt index: {} trees loaded", trees.len()),
        Err(e) => log::warn!("Index rebuild issue: {}", e),
    }

    // Scan for unterminated sessions from a previous unclean shutdown
    let unterm = store.scan_unterminated();
    for id in &unterm {
        log::info!("[lifecycle] appending synthetic session_end for unterminated tree {}", id);
        lifecycle::append_synthetic_session_end(&store, id);
        store.reset_header_tokens(id).ok();
    }
    if !unterm.is_empty() {
        log::info!("[lifecycle] cleaned up {} unterminated session(s)", unterm.len());
    }

    if let Err(e) = agent_core::hooks::run_startup_hooks() {
        log::warn!("Startup hooks issue: {}", e);
    }

    // Signal handling for graceful shutdown
    let shutting_down = Arc::new(AtomicBool::new(false));
    let sd_for_int = shutting_down.clone();
    let sd_for_term = shutting_down.clone();
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, sd_for_int) {
        log::warn!("Failed to register SIGINT handler: {}", e);
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, sd_for_term) {
        log::warn!("Failed to register SIGTERM handler: {}", e);
    }

    let bind = format!("{}:{}", config.server.host, config.server.port);
    let listener = std::net::TcpListener::bind(&bind).expect("bind");
    log::info!("Listening on http://{}", bind);
    listener.set_nonblocking(true).ok();

    loop {
        if shutting_down.load(Ordering::Relaxed) {
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
            Err(_) => continue,
        }
    }

    lifecycle::shutdown_all(&store);
    log::info!("[lifecycle] server stopped");
}

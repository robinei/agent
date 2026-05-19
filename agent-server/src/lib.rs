use std::sync::Arc;

mod http;
mod lifecycle;
mod routes;

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

    if let Err(e) = agent_core::hooks::run_startup_hooks() {
        log::warn!("Startup hooks issue: {}", e);
    }

    let bind = format!("{}:{}", config.server.host, config.server.port);
    let listener = std::net::TcpListener::bind(&bind).expect("bind");
    log::info!("Listening on http://{}", bind);

    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let store = store.clone();
        let cfg = config.clone();
        std::thread::spawn(move || http::handle_connection(stream, store, cfg));
    }
}

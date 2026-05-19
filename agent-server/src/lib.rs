use std::sync::Arc;

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

    let addr = format!("{}:{}", config.server.host, config.server.port);
    log::info!("Listening on http://{}", addr);

    let store_for_server = store.clone();
    let config_for_server = config.clone();
    rouille::start_server(addr, move |request| {
        let store = store_for_server.clone();
        let config = config_for_server.clone();
        routes::handle_request(request, &store, &config)
    });
}
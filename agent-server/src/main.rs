use std::sync::Arc;

use agent_core::config::load_config;
use agent_core::logging::init_logging;
use agent_core::store::Store;

mod lifecycle;
mod routes;

fn main() {
    // 1. Load config
    let config = load_config();

    // 2. Init logging
    let log_file = config
        .logging
        .to_file
        .as_ref()
        .and_then(|p| p.to_str());
    init_logging(log_file, &config.logging.level);

    log::info!("Starting agent-server...");
    log::info!(
        "Config: host={}, port={}, model={}",
        config.server.host,
        config.server.port,
        config.provider.model
    );

    // 3. Initialize store
    let store = Arc::new(Store::default());

    // 4. Rebuild index from disk
    match store.rebuild_index() {
        Ok(trees) => log::info!("Rebuilt index: {} trees loaded", trees.len()),
        Err(e) => log::warn!("Index rebuild issue: {}", e),
    }

    // 5. Start HTTP server
    let addr = format!("{}:{}", config.server.host, config.server.port);
    log::info!("Listening on http://{}", addr);

    let store_for_server = store.clone();
    rouille::start_server(addr, move |request| {
        let store = store_for_server.clone();
        routes::handle_request(request, &store)
    });
}

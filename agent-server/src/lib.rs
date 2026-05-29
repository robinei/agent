//! Agent server — axum-based async HTTP/WS server.
//!
//! Phase 2: runs on a `current_thread` tokio runtime via `block_on`.
//! No hand-rolled HTTP, no nix::poll, no signal_hook.

use std::sync::Arc;

use agent_core::config::Config;

pub mod llm_client;
pub mod provider;
pub mod sandbox;
pub mod server;
pub mod spawner;
pub mod worker_task;

/// Initialise shared state: index rebuild, session recovery, startup hooks.
/// Does not bind any socket or register signal handlers.
/// Called by both `run()` and the CLI's embedded boot path.
pub async fn embed_init(config: Arc<Config>, to_stderr: bool) {
    let log_file = config.logging.to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging.level, to_stderr);

    match agent_core::tree_io::list_trees(&agent_core::config::agent_dir()).await {
        Ok(trees) => log::info!("Rebuilt index: {} trees loaded", trees.len()),
        Err(e) => log::warn!("Index rebuild issue: {}", e),
    }
}

/// Serve on an already-bound TCP listener. Used by both `run_async` and
/// the CLI's embedded server path.
pub async fn serve_on(listener: tokio::net::TcpListener, config: Arc<Config>) {
    embed_init(config.clone(), config.logging.to_stderr).await;

    let local_addr = listener.local_addr().ok();
    log::info!("Starting agent-server on {:?}...", local_addr);
    log::info!(
        "Config: host={}, port={}, model={}",
        config.server.host,
        config.server.port,
        config.provider.model
    );

    let app_state = server::AppState {
        workers: std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        llm: llm_client::LlmClient::new(),
        cfg: config.clone(),
    };

    let app = server::build_router(app_state);

    // Shutdown signal handling
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // SIGINT
    #[cfg(unix)]
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                log::info!("[lifecycle] SIGINT received, shutting down");
                let _ = tx.send(true);
            }
        });
    }

    // SIGTERM
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("Failed to register SIGTERM handler: {}", e);
                    return;
                }
            };
            term.recv().await;
            log::info!("[lifecycle] SIGTERM received, shutting down");
            let _ = tx.send(true);
        });
    }

    // Serve with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            loop {
                if *shutdown_rx.borrow_and_update() {
                    break;
                }
                shutdown_rx.changed().await.ok();
            }
        })
        .await
        .ok();

    spawner::shutdown_all().await;
    log::info!("[lifecycle] server stopped");
}

/// Run the async server. Binds to the configured host:port and calls `serve_on`.
pub async fn run_async(config: Arc<Config>) {
    let bind = format!("{}:{}", config.server.host, config.server.port);
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("Failed to bind to {}: {}", bind, e);
            return;
        }
    };
    serve_on(listener, config).await;
}

/// Sync entry point. Creates a `current_thread` tokio runtime and blocks
/// on it. Used by the binary's `main()` and by `agent-cli` embedded path.
pub fn serve(cfg: Arc<Config>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(run_async(cfg))
}

/// CLI entry point — parses args, loads config, runs the server.
pub fn run(args: Vec<String>) {
    let _ = args;
    let config = Arc::new(agent_core::config::load_config());
    serve(config);
}

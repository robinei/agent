//! Axum HTTP/WebSocket server replacing the hand-rolled `http.rs` + `ws.rs`.
//!
//! Phase 2 of the async migration: routes are axum handlers, WS uses axum's
//! WebSocketUpgrade, and the worker is an async task (not a thread + nix::poll).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::Router;
use tokio::sync::{broadcast, mpsc, Mutex};

use agent_core::config::Config;
use agent_core::rpc::WsCommand;
use agent_core::tree_io;
use agent_core::types::{ServerEvent, TreeMeta, TreeSandbox};

use crate::llm_client::LlmClient;

// ── App state ──

/// Shared application state, cloned into every handler.
#[derive(Clone)]
pub struct AppState {
    pub workers: Arc<Mutex<HashMap<String, WorkerHandle>>>,
    pub llm: LlmClient,
    pub cfg: Arc<Config>,
}

/// Per-worker handles for the server to communicate with the worker task.
#[derive(Clone)]
pub struct WorkerHandle {
    /// Send WsCommands to the worker (e.g. Message, Stop, GetEntries, AutoTitle).
    pub cmd_tx: mpsc::Sender<WsCommand>,
    /// Subscribe to ServerEvents broadcast from the worker.
    pub ev_tx: broadcast::Sender<ServerEvent>,
    /// Spawn token to abort the worker task.
    pub abort: tokio::sync::watch::Sender<bool>,
}

impl WorkerHandle {
    pub fn new(
        cmd_tx: mpsc::Sender<WsCommand>,
        ev_tx: broadcast::Sender<ServerEvent>,
    ) -> Self {
        let (abort, _) = tokio::sync::watch::channel(false);
        Self { cmd_tx, ev_tx, abort }
    }
}

// ── Router ──

/// Build the axum Router with all routes and shared state.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/trees", get(list_trees).post(create_tree))
        .route("/trees/{id}", get(get_tree).patch(update_tree))
        .route("/trees/{id}/stop", post(stop_agent))
        .route("/trees/{id}/ws", get(ws_handler))
        .with_state(state)
}

// ── REST handlers ──

async fn root() -> impl IntoResponse {
    axum::Json(serde_json::json!({"service": "agent-server", "version": "0.1.0"}))
}

async fn list_trees() -> impl IntoResponse {
    match tree_io::list_trees(&agent_core::config::agent_dir()).await {
        Ok(trees) => (StatusCode::OK, axum::Json(trees)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn create_tree(
    State(state): State<AppState>,
    axum::extract::Json(body): axum::extract::Json<CreateTreeBody>,
) -> impl IntoResponse {
    let tree_id = uuid::Uuid::new_v4().to_string();
    let sandbox = body.sandbox.unwrap_or_default();

    let repo_path = match &body.repo_path {
        Some(p) => {
            let path = std::path::Path::new(p);
            match agent_core::types::validate_repo_path(
                path,
                &state.cfg.sandbox.defaults.hide,
                &sandbox,
            ) {
                Ok(canon) => Some(canon),
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            }
        }
        None => None,
    };

    let now = chrono::Utc::now().timestamp();
    let meta = TreeMeta {
        id: tree_id.clone(),
        parent_id: None,
        repo_path,
        title: body.title,
        created_at: now,
        updated_at: now,
        leaf_id: None,
        sandbox,
    };

    match tree_io::create_tree(&agent_core::config::agent_dir(), &meta).await {
        Ok(()) => (StatusCode::CREATED, axum::Json(meta)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("failed to create tree: {}", e)})),
        )
            .into_response(),
    }
}

async fn get_tree(Path(id): Path<String>) -> impl IntoResponse {
    match tree_io::read_meta(&agent_core::config::agent_dir(), &id).await {
        Ok(Some(meta)) => (StatusCode::OK, axum::Json(meta)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": format!("tree {} not found", id)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn update_tree(
    Path(id): Path<String>,
    axum::extract::Json(body): axum::extract::Json<UpdateTreeBody>,
) -> impl IntoResponse {
    let mut meta = match tree_io::read_meta(&agent_core::config::agent_dir(), &id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({"error": format!("tree {} not found", id)})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    if let Some(title) = body.title {
        meta.title = Some(title);
    }
    if let Some(sandbox) = body.sandbox {
        meta.sandbox = sandbox;
    }
    meta.updated_at = chrono::Utc::now().timestamp();

    match tree_io::write_meta(&agent_core::config::agent_dir(), &meta).await {
        Ok(()) => (StatusCode::OK, axum::Json(meta)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("failed to update tree: {}", e)})),
        )
            .into_response(),
    }
}

async fn stop_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let workers = state.workers.lock().await;
    match workers.get(&id) {
        Some(handle) => {
            let _ = handle.cmd_tx.send(WsCommand::Stop).await;
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"status": "stopping"})),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": format!("no active worker for {}", id)})),
        )
            .into_response(),
    }
}

// ── WebSocket handler ──

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, id, state))
}

async fn handle_ws(mut socket: WebSocket, tree_id: String, state: AppState) {
    // Get or spawn the worker for this tree.
    let handle = {
        let mut workers = state.workers.lock().await;
        if !workers.contains_key(&tree_id) {
            let new_handle = crate::spawner::spawn_worker(&tree_id, state.cfg.clone()).await;
            workers.insert(tree_id.clone(), new_handle);
        }
        workers.get(&tree_id).cloned().unwrap()
    };

    // Subscribe to broadcasts BEFORE issuing GetEntries so we don't miss events.
    // INTENTIONAL: subscribing first means the live stream may overlap replayed
    // history; the client de-dupes by entry id, so overlap is harmless, a gap is not.
    let mut ev_rx = handle.ev_tx.subscribe();

    // Send GetEntries to replay history for this new client.
    let _ = handle
        .cmd_tx
        .send(WsCommand::GetEntries { count: None })
        .await;

    // Event loop: select! over WS recv and broadcast recv.
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<WsCommand>(&text) {
                            let _ = handle.cmd_tx.send(cmd).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_))) => {
                        // Binary frames not used by the protocol; ignore.
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Err(e)) => {
                        log::debug!("[ws] {} WS error: {}", tree_id, e);
                        break;
                    }
                }
            }
            ev = ev_rx.recv() => {
                match ev {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("[ws] {} client lagged by {} events — disconnecting", tree_id, n);
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Serve the axum app over a single TCP stream (used by CLI embedded session).
///
/// Wraps the existing `Router` and runs it with hyper over one connection,
/// then returns. This is for the socketpair case — no HTTP server, just
/// one request-response cycle (or WS upgrade).
pub async fn serve_single_connection(
    stream: tokio::net::TcpStream,
    config: Arc<Config>,
) {
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tower::Service;

    let app_state = crate::server::AppState {
        workers: std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        llm: crate::llm_client::LlmClient::new(),
        cfg: config,
    };
    let mut router = crate::server::build_router(app_state);

    // hyper is the underlying HTTP layer — axum wraps it. We use
    // service_fn to adapt the axum Router as a tower Service.
    let svc = service_fn(|req: axum::http::Request<Incoming>| {
        let mut router = router.clone();
        async move { router.call(req).await }
    });

    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), svc)
        .await;
}

// ── Request body types ──

#[derive(serde::Deserialize)]
pub struct CreateTreeBody {
    pub title: Option<String>,
    pub repo_path: Option<String>,
    pub sandbox: Option<TreeSandbox>,
}

#[derive(serde::Deserialize)]
pub struct UpdateTreeBody {
    pub title: Option<String>,
    pub sandbox: Option<TreeSandbox>,
}

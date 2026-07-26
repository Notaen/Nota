use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Request,
    http::StatusCode,
    middleware,
    response::Response,
    routing::get,
};
use nota_core::bus::EventBus;
use nota_core::permissions::PermissionRegistry;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::http::api::ApiState;
use crate::http::ws::WsState;

pub(crate) mod admin;
pub(crate) mod api;
pub(crate) mod ws;

async fn log_request(req: Request, next: middleware::Next) -> Response {
    log::debug!("Received: {} {}", req.method(), req.uri());
    next.run(req).await
}

#[derive(Serialize)]
struct Health<'a> {
    version: &'a str,
}

async fn health() -> (StatusCode, Json<Health<'static>>) {
    (
        StatusCode::OK,
        Json(Health {
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
}

pub struct AppContext {
    pub bus: Arc<EventBus>,
    pub permissions: Arc<PermissionRegistry>,
    pub api_state: Arc<ApiState>,
}

pub fn router(ctx: Arc<AppContext>, cancel_token: CancellationToken) -> Router {
    let api_routes = Router::new()
        .route("/health", get(health))
        .nest("/admin", admin::router(cancel_token.clone()))
        .nest("/api", api::router())
        .with_state(ctx.api_state.clone())
        .layer(middleware::from_fn(log_request));

    let ws_state = Arc::new(WsState {
        bus: ctx.bus.clone(),
        permissions: ctx.permissions.clone(),
    });

    let ws_routes = Router::new()
        .route("/ws/chat", get(ws::ws_chat_handler))
        .with_state(ws_state);

    api_routes.merge(ws_routes)
}

pub async fn serve(
    listener: TcpListener,
    ctx: Arc<AppContext>,
    cancel_token: CancellationToken,
) {
    let app = router(ctx, cancel_token.clone());
    log::debug!("Server listening on {}", listener.local_addr().unwrap());

    let shutdown_future = async move {
        cancel_token.cancelled().await;
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_future)
        .await
        .unwrap();
}

pub fn find_static_dir(start: &PathBuf) -> Option<PathBuf> {
    let candidates = [
        start.join("webui"),
        PathBuf::from("webui/dist"),
        PathBuf::from("../webui/dist"),
    ];
    candidates.into_iter().find(|p| p.join("index.html").exists())
}

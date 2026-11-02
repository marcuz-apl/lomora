use std::sync::Arc;
use axum::body::Body;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use crate::config::Config;

#[derive(Clone)]
pub struct AppState { pub cfg: Arc<Config> }

impl AppState {
    /// The single construction path. Tasks 2, 3, 4 and 6 each add one field here — to the
    /// struct, to `new`, and to no other call site, because `main.rs`, `spawn_app`, and
    /// `fresh_app` all go through this function.
    pub fn new(cfg: Config) -> AppState { AppState { cfg: Arc::new(cfg) } }
}

async fn fallback(State(st): State<AppState>, uri: Uri) -> Response<Body> {
    let path = uri.path().to_string();
    match &st.cfg.static_dir {
        Some(dir) => crate::assets::serve_dir(dir.clone(), Some(path)).await,
        None => crate::assets::serve(Some(path)).await,
    }
}

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);
    Router::new()
        .route("/api/history", get(crate::routes::get_history))
        .route("/api/test", post(crate::routes::run_test_placeholder))
        .fallback(fallback)
        .layer(cors)
        .with_state(state)
}

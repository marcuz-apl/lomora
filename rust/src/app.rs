use std::sync::Arc;
use axum::body::Body;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use crate::config::Config;
use crate::store::Db;

#[derive(Clone)]
pub struct AppState { pub cfg: Arc<Config>, pub hw: crate::hw::Sampler, pub http: reqwest::Client,
                      pub db: Db, pub sys: crate::hw::ProfileStatic }

impl AppState {
    /// The single construction path. Tasks 2, 3, 4 and 6 each add one field here — to the
    /// struct, to `new`, and to no other call site, because `main.rs`, `spawn_app`, and
    /// `fresh_app` all go through this function. Task 4 adds `db` (the SQLite store) and
    /// `sys` (the profile facts captured once at startup, ruling R4).
    pub fn new(cfg: Config, hw: crate::hw::Sampler, http: reqwest::Client,
               db: Db, sys: crate::hw::ProfileStatic) -> AppState {
        AppState { cfg: Arc::new(cfg), hw, http, db, sys }
    }
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
        .route("/api/hardware", get(crate::routes::hardware))
        .route("/api/models", get(crate::routes::list_models))
        .route("/api/profile", get(crate::routes::get_profile).post(crate::routes::set_profile))
        .route("/api/history", get(crate::routes::get_history))
        .route("/api/test", post(crate::routes::run_test))
        .fallback(fallback)
        .layer(cors)
        .with_state(state)
}

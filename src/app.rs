use std::sync::Arc;
use axum::body::Body;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use crate::config::{Config, SqlGate};
use crate::store::Db;

#[derive(Clone)]
pub struct AppState { pub cfg: Arc<Config>, pub hw: crate::hw::Sampler, pub http: reqwest::Client,
                      pub db: Db, pub sys: crate::hw::ProfileStatic, pub gpu: crate::hw::GpuHandle,
                      /// Live state of the SQL console; the tray's Settings menu writes it.
                      pub sql: SqlGate }

impl AppState {
    /// The single construction path. Every field is wired here and nowhere else, because
    /// `main.rs`, `spawn_app`, and `fresh_app` all go through this function. `db` is the SQLite
    /// store, `sys` the profile facts captured once at startup, and `gpu` the shared GPU slot
    /// the sampler reads — one handle, so a card detected after startup is visible to both the
    /// hardware snapshot and the profile endpoint.
    pub fn new(cfg: Config, hw: crate::hw::Sampler, http: reqwest::Client,
               db: Db, sys: crate::hw::ProfileStatic, gpu: crate::hw::GpuHandle) -> AppState {
        // The environment decides where the console starts; this cell is what the routes and
        // the tray share from then on.
        let sql = SqlGate::new(cfg.sql_console);
        AppState { cfg: Arc::new(cfg), hw, http, db, sys, gpu, sql }
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
        .route("/api/version", get(crate::routes::version))
        .route("/api/models", get(crate::routes::list_models))
        .route("/api/profile", get(crate::routes::get_profile).post(crate::routes::set_profile))
        .route("/api/history", get(crate::routes::get_history))
        .route("/api/history/{id}", delete(crate::routes::delete_run))
        .route("/api/test", post(crate::routes::run_test))
        .route("/api/stress", post(crate::routes::stress_test))
        .route("/api/prompts", get(crate::routes::get_prompts))
        .route("/api/prompts/fetch", post(crate::routes::fetch_prompts))
        // Both halves of the console check the live `AppState::sql` gate inside the handlers, not
        // here: the routes exist in every build, so a closed console answers its documented
        // refusal instead of a 404 the dashboard would have to guess about.
        .route("/api/sql", post(crate::routes::run_sql))
        .route("/api/sql/backup", get(crate::routes::sql_backup))
        .fallback(fallback)
        .layer(cors)
        .with_state(state)
}

#![allow(dead_code)]
// The mock lives in the library (src/mock.rs) so src/bin/mock_upstream.rs can serve the exact
// same behaviour as a separate process; tests/ re-exports it instead of defining it.
// Task 6 adds `spawn_limited_mock` and `peak_inflight` to this list — they do not exist yet,
// and re-exporting them here would make every test binary fail to resolve the import.
// `unused_imports` is allowed because this module is compiled into every test binary, and
// binaries that only call `spawn_app` would otherwise fail to build for wanting a mock.
#[allow(unused_imports)]
pub use lomora::mock::{spawn_mock, spawn_mock_sparse};

/// Binds port 0, lets the OS pick an ephemeral port, and returns the base URL plus the
/// server task. Every later task's tests go through here.
pub async fn spawn_app() -> (String, tokio::task::JoinHandle<()>) {
    // `Db::in_memory()` is an in-memory SQLite database: each test that calls `spawn_app` gets
    // its own isolated profile row, no shared file, no cross-test interference, and nothing
    // written into the repo.
    spawn_app_on_db(lomora::store::Db::in_memory()).await
}

/// The same app over a caller-supplied `Db`, so a test can install a handle whose writes fail
/// (the profile error-envelope test) or open a real file twice for a restart (Task 7). Everything
/// else about the app is identical to `spawn_app`.
pub async fn spawn_app_on_db(db: lomora::store::Db) -> (String, tokio::task::JoinHandle<()>) {
    spawn_app_with(lomora::config::Config::from_env(), db).await
}

/// `spawn_app` uses `Config` defaults, which point at a dead LM Studio; `fresh_app(mock)` builds
/// the same app whose `lmstudio_url` IS the mock, so a test that only cares about the benchmark
/// needs no `base_url` juggling — while the explicit-`base_url` assertions stay anyway, because
/// they mirror real usage. One construction path shared with `spawn_app`/`spawn_app_on_db`
/// (and `main.rs` through `AppState::new`), so the copies cannot drift.
pub async fn fresh_app(mock: &str) -> (String, tokio::task::JoinHandle<()>) {
    let mut cfg = lomora::config::Config::from_env();
    cfg.lmstudio_url = mock.to_string();
    spawn_app_with(cfg, lomora::store::Db::in_memory()).await
}

async fn spawn_app_with(cfg: lomora::config::Config, db: lomora::store::Db)
    -> (String, tokio::task::JoinHandle<()>)
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .await.unwrap();
    let addr = listener.local_addr().unwrap();
    // NoGpu keeps the endpoint deterministic: the GPU keys are then absent, and hardware
    // presence is covered by the live parity sweep, not by the test suite.
    let gpu = std::sync::Arc::new(lomora::hw::NoGpu);
    let hw = lomora::hw::Sampler::spawn(gpu, std::time::Duration::from_millis(60)).await;
    // Same shared-client shape as main.rs, so tests exercise the pooled path rather than a
    // per-request client.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(64).build().unwrap();
    // `read_profile_static()` runs once per app, mirroring `main.rs` (ruling R4) — the profile
    // fields in test responses are this machine's real values, which is what
    // `default_profile_name_is_my_workstation` asserts.
    let sys = lomora::hw::read_profile_static();
    let state = lomora::app::AppState::new(cfg, hw, http, db, sys);
    let handle = tokio::spawn(async move {
        axum::serve(listener, lomora::app::build_router(state)).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

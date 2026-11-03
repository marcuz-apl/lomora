#![allow(dead_code)]
// The mock lives in the library (src/mock.rs) so src/bin/mock_upstream.rs can serve the exact
// same behaviour as a separate process; tests/ re-exports it instead of defining it. The
// concurrency-counting mock below has no out-of-process consumer, so it stays test-side.
// `unused_imports` is allowed because this module is compiled into every test binary, and
// binaries that only call `spawn_app` would otherwise fail to build for wanting a mock.
#[allow(unused_imports)]
pub use lomora::mock::{spawn_mock, spawn_mock_models, spawn_mock_sparse};

/// Binds port 0, lets the OS pick an ephemeral port, and returns the base URL plus the
/// server task. Every later task's tests go through here.
pub async fn spawn_app() -> (String, tokio::task::JoinHandle<()>) {
    // `Db::in_memory()` is an in-memory SQLite database: each test that calls `spawn_app` gets
    // its own isolated profile row, no shared file, no cross-test interference, and nothing
    // written into the repo.
    spawn_app_on_db(lomora::store::Db::in_memory()).await
}

/// The same app over a caller-supplied `Db`, so a test can install a handle whose writes fail
/// (the profile error-envelope test) or open a real file twice for a restart. Everything else
/// about the app is identical to `spawn_app`.
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

/// `fresh_app` over a real file instead of `Db::in_memory()`, so the restart test can build a
/// second app over the same path after the first one has exited. The path is passed explicitly
/// rather than through `LOMORA_DB`: `Config` is read once per app, and an env var written by one
/// test would leak into every other test in this binary (`set_var` is unsafe in edition 2024
/// precisely because of that cross-thread hazard).
pub async fn fresh_app_on_db(mock: &str, path: &std::path::Path)
    -> (String, tokio::task::JoinHandle<()>)
{
    let mut cfg = lomora::config::Config::from_env();
    cfg.lmstudio_url = mock.to_string();
    spawn_app_with(cfg, lomora::store::Db::open(path).expect("test database")).await
}

/// A fully caller-built `Config` over an in-memory database — for tests that configure a whole
/// subsystem (the prompt pool's sources) without touching the process environment.
pub async fn fresh_app_cfg(cfg: lomora::config::Config) -> (String, tokio::task::JoinHandle<()>) {
    spawn_app_with(cfg, lomora::store::Db::in_memory()).await
}

/// `fresh_app_cfg` over a real file, so the prompt-cache restart test can open the same path twice.
pub async fn fresh_app_cfg_on_db(cfg: lomora::config::Config, path: &std::path::Path)
    -> (String, tokio::task::JoinHandle<()>)
{
    spawn_app_with(cfg, lomora::store::Db::open(path).expect("test database")).await
}

/// A caller-built router on an ephemeral port — for mocks that need several routes with
/// caller-chosen bodies (the multi-format benchmark-source mock), which the fixed mocks in
/// `src/mock.rs` cannot express.
pub async fn spawn_router(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    (format!("http://{addr}"), handle)
}

async fn spawn_app_with(cfg: lomora::config::Config, db: lomora::store::Db)
    -> (String, tokio::task::JoinHandle<()>)
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .await.unwrap();
    let addr = listener.local_addr().unwrap();
    // NoGpu keeps the endpoints deterministic: `/api/hardware` omits its GPU keys and
    // `/api/profile` answers the no-NVML branch, `"gpu": "None detected"`. Real hardware
    // presence is covered by a manual run against the dev machine, not by the test suite.
    let gpu: std::sync::Arc<dyn lomora::hw::Gpu> = std::sync::Arc::new(lomora::hw::NoGpu);
    let hw = lomora::hw::Sampler::spawn(gpu.clone(), std::time::Duration::from_millis(60)).await;
    // Same shared-client shape as main.rs, so tests exercise the pooled path rather than a
    // per-request client.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(64).build().unwrap();
    // `read_profile_static()` runs once per app, mirroring `main.rs` — the profile fields in
    // test responses are this machine's real values, which is what
    // `default_profile_name_is_my_workstation` asserts.
    let sys = lomora::hw::read_profile_static();
    let state = lomora::app::AppState::new(cfg, hw, http, db, sys, gpu);
    let handle = tokio::spawn(async move {
        axum::serve(listener, lomora::app::build_router(state)).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

/// A mock that counts simultaneous in-flight completions and reports the peak at `/__peak`.
/// `spawn_mock` answers instantly, so the semaphore test could pass without ever overlapping
/// two requests; this one sleeps `delay_ms` per completion, which makes concurrency observable.
/// It stays here, not in `src/mock.rs`: the out-of-process `mock_upstream` binary needs the
/// other routers, not this one, so this scaffolding should not ship in the library.
pub async fn spawn_limited_mock(delay_ms: u64) -> (String, tokio::task::JoinHandle<()>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let peak_reader = peak.clone();
    let inflight_handler = inflight.clone();
    let peak_handler = peak.clone();
    let app = axum::Router::new()
        .route("/__peak", axum::routing::get(move || {
            let seen = peak_reader.load(Ordering::SeqCst);
            async move { axum::Json(serde_json::json!({ "peak": seen })) }
        }))
        .route("/v1/chat/completions", axum::routing::post(move |_body: String| {
            let (inflight, peak) = (inflight_handler.clone(), peak_handler.clone());
            async move {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({
                    "choices": [{"message": {"content": "hi"}}],
                    "usage": {"completion_tokens": 20}
                }))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    (format!("http://{addr}"), handle)
}

/// The peak number of simultaneously in-flight completions the limited mock has seen.
pub async fn peak_inflight(base: &str) -> u64 {
    let v: serde_json::Value = reqwest::get(format!("{base}/__peak"))
        .await.expect("the limited mock must be up").json().await.unwrap();
    v["peak"].as_u64().expect("peak must be a number")
}

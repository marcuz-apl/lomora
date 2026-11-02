#![allow(dead_code)]
// The mock lives in the library (src/mock.rs) so src/bin/mock_upstream.rs can serve the exact
// same behaviour as a separate process; tests/ re-exports it instead of defining it.
// Task 6 adds `spawn_limited_mock` and `peak_inflight` to this list — they do not exist yet,
// and re-exporting them here would make every test binary fail to resolve the import.
// `unused_imports` is allowed because this module is compiled into every test binary, and
// binaries that only call `spawn_app` would otherwise fail to build for wanting a mock.
#[allow(unused_imports)]
pub use lomora::mock::{spawn_mock, spawn_mock_sparse};
use std::net::SocketAddr;
use tokio::net::TcpListener;

/// Binds port 0, lets the OS pick an ephemeral port, and returns the base URL plus the
/// server task. Every later task's tests go through here.
pub async fn spawn_app() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = lomora::config::Config::from_env();
    // NoGpu keeps the endpoint deterministic: the GPU keys are then absent, and hardware
    // presence is covered by the live parity sweep, not by the test suite.
    let gpu = std::sync::Arc::new(lomora::hw::NoGpu);
    let hw = lomora::hw::Sampler::spawn(gpu, std::time::Duration::from_millis(60)).await;
    // Same shared-client shape as main.rs, so tests exercise the pooled path rather than a
    // per-request client.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(64).build().unwrap();
    // `Db::default_profile()` is an in-memory SQLite database: each test that calls
    // `spawn_app` gets its own isolated profile row, no shared file, no cross-test
    // interference, and nothing written into the repo. `read_profile_static()` runs once per
    // app, mirroring `main.rs` (ruling R4) — the profile fields in test responses are this
    // machine's real values, which is what `default_profile_name_is_my_workstation` asserts.
    let db = lomora::store::Db::default_profile();
    let sys = lomora::hw::read_profile_static();
    let state = lomora::app::AppState::new(cfg, hw, http, db, sys);
    let handle = tokio::spawn(async move {
        axum::serve(listener, lomora::app::build_router(state)).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

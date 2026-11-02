#![allow(dead_code)]
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
    let state = lomora::app::AppState::new(cfg, hw);
    let handle = tokio::spawn(async move {
        axum::serve(listener, lomora::app::build_router(state)).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

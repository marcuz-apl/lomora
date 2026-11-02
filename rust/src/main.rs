use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = lomora::config::Config::from_env();
    let port = cfg.port;
    // GPU index 0 only, like Python's nvmlDeviceGetHandleByIndex(0); NVML absent -> NoGpu,
    // which answers with the GPU keys missing rather than with nulls.
    let gpu: std::sync::Arc<dyn lomora::hw::Gpu> = match lomora::hw::NvmlGpu::open(0) {
        Some(g) => std::sync::Arc::new(g),
        None => std::sync::Arc::new(lomora::hw::NoGpu),
    };
    let hw = lomora::hw::Sampler::spawn(gpu, std::time::Duration::from_millis(500)).await;
    // One client for the whole process: Python builds a fresh `httpx.AsyncClient` per request,
    // which throws away the connection pool every time. The 10 s model-list and streaming
    // benchmark overrides live at the call sites; this ceiling is the long-run upper bound.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(64).build().unwrap();
    let state = lomora::app::AppState::new(cfg, hw, http);
    let router = lomora::app::build_router(state);
    let addr = format!("0.0.0.0:{port}");
    println!("Lomora listening on http://{addr}");
    axum::serve(TcpListener::bind(&addr).await?, router).await?;
    Ok(())
}

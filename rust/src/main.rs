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
    let state = lomora::app::AppState::new(cfg, hw);
    let router = lomora::app::build_router(state);
    let addr = format!("0.0.0.0:{port}");
    println!("Lomora listening on http://{addr}");
    axum::serve(TcpListener::bind(&addr).await?, router).await?;
    Ok(())
}

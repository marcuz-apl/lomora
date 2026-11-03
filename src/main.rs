use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = lomora::config::Config::from_env();
    let port = cfg.port;
    // GPU index 0 only; NVML absent -> NoGpu, which answers with the GPU keys missing rather
    // than with nulls.
    let gpu: std::sync::Arc<dyn lomora::hw::Gpu> = match lomora::hw::NvmlGpu::open(0) {
        Some(g) => std::sync::Arc::new(g),
        None => std::sync::Arc::new(lomora::hw::NoGpu),
    };
    // Same handle for the sampler and `GET /api/profile`: one NVML context per process.
    let hw = lomora::hw::Sampler::spawn(gpu.clone(), std::time::Duration::from_millis(500)).await;
    // One client for the whole process, so the connection pool is reused. The 10 s model-list
    // and streaming benchmark overrides live at the call sites; this ceiling is the long-run
    // upper bound.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(64).build().unwrap();
    // SQLite keeps the profile row across restarts, where an in-memory profile would reset to
    // defaults. `Db::open` must read `cfg.db_path` before `cfg` moves into `AppState::new`.
    let db = lomora::store::Db::open(&cfg.db_path)?;
    // Profile facts (CPU brand, core counts, OS string) are read once here, never per
    // request.
    let sys = lomora::hw::read_profile_static();
    let state = lomora::app::AppState::new(cfg, hw, http, db, sys, gpu);
    let router = lomora::app::build_router(state);
    let addr = format!("0.0.0.0:{port}");
    println!("Lomora listening on http://{addr}");
    axum::serve(TcpListener::bind(&addr).await?, router).await?;
    Ok(())
}

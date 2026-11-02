use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = lomora::config::Config::from_env();
    let port = cfg.port;
    let state = lomora::app::AppState::new(cfg);
    let router = lomora::app::build_router(state);
    let addr = format!("0.0.0.0:{port}");
    println!("Lomora listening on http://{addr}");
    axum::serve(TcpListener::bind(&addr).await?, router).await?;
    Ok(())
}

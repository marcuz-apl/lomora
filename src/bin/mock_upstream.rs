//! Standalone mock upstream process. Running it over the network exercises the same routes the
//! in-process tests bind, from a separate OS process, so it answers from
//! `lomora::mock::router()` — the same construction the in-process tests use.
//!
//! Run: `cargo run --bin mock_upstream [port]` (default `127.0.0.1:8600`).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8600);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("mock upstream on http://127.0.0.1:{port}");
    axum::serve(listener, lomora::mock::router()).await?;
    Ok(())
}

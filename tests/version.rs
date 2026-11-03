//! `GET /api/version` is how the dashboard's About dialog learns which build it is running.
//! The value is baked in from the manifest at compile time, so `Cargo.toml` stays the only
//! place a version is written — the page fetches it instead of carrying a second copy that
//! could drift.

mod support;

use serde_json::Value;

#[tokio::test]
async fn version_endpoint_answers_the_crate_version() {
    let (base, h) = support::spawn_app().await;
    let r = reqwest::get(format!("{base}/api/version")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/json");
    let v: Value = r.json().await.unwrap();
    // The integration test belongs to the same package, so `CARGO_PKG_VERSION` here is the
    // version the server binary was compiled with too — a mismatch means the endpoint invented
    // a number rather than reading the manifest.
    assert_eq!(v, serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }));
    h.abort();
}

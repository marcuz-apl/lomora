mod support;
use support::spawn_app;
use axum::body::Body;

/// The dashboard is the only client, and it is served embedded (`rust-embed` from `static/`)
/// at the site root, so the paths are `/` and `/index.html`, not `/static/index.html` (which
/// is a 404). `rel_path` reproduces the root mount, and both spellings must hand the browser
/// the same bytes.
#[tokio::test]
async fn root_and_index_serve_the_same_html() {
    let (base, _h) = spawn_app().await;
    let root = reqwest::get(format!("{base}/")).await.unwrap();
    let explicit = reqwest::get(format!("{base}/index.html")).await.unwrap();
    for (name, resp) in [("/", &root), ("/index.html", &explicit)] {
        assert_eq!(resp.status(), 200, "{name}");
        let ct = resp.headers()["content-type"].to_str().unwrap();
        assert_eq!(ct, "text/html; charset=utf-8",
            "{name}: every text/* response carries the charset suffix");
    }
    let body = root.text().await.unwrap();
    assert_eq!(body, explicit.text().await.unwrap(), "/ and /index.html must not diverge");
    assert!(body.contains("<title>Lomora - LLM Monitor</title>"), "the real dashboard, not a stub");
    // The mount is at the root, so `/static/index.html` resolves to `static/static/index.html`
    // and is a 404, not an alternate entry point.
    let mounted = reqwest::get(format!("{base}/static/index.html")).await.unwrap();
    assert_eq!(mounted.status(), 404, "the mount is at the root, so the prefixed path 404s");
}

/// An unknown path answers a plain-text 404. The dashboard never requests one, and a JSON
/// client that checks `resp.ok` treats both shapes identically, so the plain-text body is pinned
/// here as a decision, not drift.
#[tokio::test]
async fn unknown_path_is_a_plain_text_404() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/nope.txt")).await.unwrap();
    assert_eq!(resp.status(), 404);
    let ct = resp.headers()["content-type"].to_str().unwrap();
    assert!(ct.starts_with("text/plain"), "content-type was {ct}");
    assert_eq!(resp.text().await.unwrap(), "not found");
}

/// `rel_path` rejects `..` on both separators; the guard is unit-tested against `rel_path`
/// itself, but the wire is what an attacker reaches, so pin it end-to-end. A traversal must
/// answer the same 404 as a missing file and must never leak the crate's own sources.
#[tokio::test]
async fn parent_directory_traversal_is_404_not_a_file() {
    let (base, _h) = spawn_app().await;
    for path in ["/../Cargo.toml", "/static/../Cargo.toml", "/..\\Cargo.toml"] {
        let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
        let status = resp.status();
        let body = resp.text().await.unwrap();
        assert_eq!(status, 404, "{path} must be rejected, body: {body}");
        assert!(!body.contains("[package]"), "{path} leaked a source file");
    }
}

/// `LOMORA_STATIC` (a process env var) swaps the embedded assets for an on-disk directory with
/// no query-string switch. The env var itself cannot be set from a test in edition 2024
/// (`set_var` is unsafe and process-global), so this drives `serve_dir` directly and compares
/// it with the embedded `serve` for the two paths the dashboard uses. A live run against a
/// real process covers the env-var plumbing.
#[tokio::test]
async fn on_disk_mode_serves_the_same_bytes_as_the_embedded_assets() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    for path in ["/", "/index.html"] {
        let embedded = lomora::assets::serve(Some(path.to_string())).await;
        let on_disk = lomora::assets::serve_dir(dir.clone(), Some(path.to_string())).await;
        assert_eq!(embedded.status(), on_disk.status(), "{path}");
        assert_eq!(
            body_bytes(embedded).await,
            body_bytes(on_disk).await,
            "{path}: an on-disk page must be byte-identical to the embedded one",
        );
    }
}

async fn body_bytes(resp: axum::response::Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()
}

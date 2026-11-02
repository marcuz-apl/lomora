use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../static"]
pub struct Asset;

/// `StaticFiles(directory=..., html=True)` answers a bare "/" with index.html, so a path
/// that trims down to nothing is the directory root rather than a missing file.
fn rel_path(path: Option<String>) -> String {
    let trimmed = path.unwrap_or_default();
    let trimmed = trimmed.trim_start_matches('/');
    if trimmed.is_empty() { "index.html".to_string() } else { trimmed.to_string() }
}

pub async fn serve(path: Option<String>) -> Response<Body> {
    let rel = rel_path(path);
    if let Some(f) = Asset::get(&rel) {
        let ctype = mime_guess::from_path(&rel).first_or_text_plain().to_string();
        return (StatusCode::OK, [("content-type", ctype)], String::from_utf8_lossy(f.data.as_ref()).into_owned())
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

pub async fn serve_dir(dir: std::path::PathBuf, path: Option<String>) -> Response<Body> {
    let rel = rel_path(path);
    let file = dir.join(&rel);
    if file.is_file() {
        let bytes = tokio::fs::read(&file).await.unwrap_or_default();
        let ctype = mime_guess::from_path(&rel).first_or_text_plain().to_string();
        return (StatusCode::OK, [("content-type", ctype)], Body::from(bytes)).into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

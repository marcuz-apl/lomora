use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static"]
pub struct Asset;

/// `StaticFiles(directory=..., html=True)` answers a bare "/" with index.html, so a path
/// that trims down to nothing is the directory root rather than a missing file.
///
/// A path with a `..` component (on either separator) is rejected as `None`, so neither the
/// embedded mode nor the `LOMORA_STATIC` filesystem mode can walk out of the asset root; callers
/// answer `None` with the same 404 as an unknown file.
fn rel_path(path: Option<String>) -> Option<String> {
    let trimmed = path.unwrap_or_default();
    let trimmed = trimmed.trim_start_matches('/');
    if trimmed.split(['/', '\\']).any(|seg| seg == "..") {
        return None;
    }
    Some(if trimmed.is_empty() { "index.html".to_string() } else { trimmed.to_string() })
}

/// Every `text/*` response carries the `; charset=utf-8` suffix, so the dashboard's own
/// content type is unambiguous on the wire. Binary types (there are none today) pass through
/// untouched.
fn content_type(rel: &str) -> String {
    let ctype = mime_guess::from_path(rel).first_or_text_plain().to_string();
    if ctype.starts_with("text/") { format!("{ctype}; charset=utf-8") } else { ctype }
}

pub async fn serve(path: Option<String>) -> Response<Body> {
    let Some(rel) = rel_path(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if let Some(f) = Asset::get(&rel) {
        let ctype = content_type(&rel);
        return (StatusCode::OK, [("content-type", ctype)], String::from_utf8_lossy(f.data.as_ref()).into_owned())
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

pub async fn serve_dir(dir: std::path::PathBuf, path: Option<String>) -> Response<Body> {
    let Some(rel) = rel_path(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let file = dir.join(&rel);
    if file.is_file() {
        let bytes = tokio::fs::read(&file).await.unwrap_or_default();
        let ctype = content_type(&rel);
        return (StatusCode::OK, [("content-type", ctype)], Body::from(bytes)).into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

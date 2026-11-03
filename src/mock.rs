//! In-process mock upstream shared by the integration tests and the standalone mock binary.
//!
//! It answers the three shapes lomora consumes: an OpenAI model list, a non-streaming
//! completion carrying `usage`, and a streaming completion whose SSE frames are deliberately
//! split mid-line, so the scanner has to buffer across chunk boundaries instead of
//! assuming one frame per chunk.
//!
//! It lives in the library rather than under `tests/` because `src/bin/mock_upstream.rs` runs
//! it as a separate OS process, and neither a `src/bin/` nor an `examples/` target can import
//! code from `tests/`, which is not part of the crate. `tests/support/mod.rs` re-exports these
//! functions instead of defining them, so the in-process and out-of-process mocks can never
//! drift into answering differently.

use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;

/// The construction shared by `spawn_mock` and `src/bin/mock_upstream.rs`. Do not duplicate
/// these route definitions in a second place.
pub fn router() -> Router {
    Router::new()
        .route("/v1/models", get(|| async {
            Json(json!({
                "object": "list",
                "data": [{"id": "mock-a", "object": "model"}, {"id": "mock-b", "object": "model"}]
            }))
        }))
        .route("/v1/chat/completions", post(handle_completion))
}

/// Identical completion behaviour, but `/v1/models` answers with the four shapes the id
/// filter has to survive: one usable id, one missing, one empty string (the filter treats it
/// as absent), one null.
pub fn sparse_router() -> Router {
    Router::new()
        .route("/v1/models", get(|| async {
            Json(json!({"data": [
                {"id": "real-one"},
                {"object": "model"},
                {"id": "", "object": "model"},
                {"id": null, "object": "model"}
            ]}))
        }))
        .route("/v1/chat/completions", post(handle_completion))
}

pub async fn spawn_mock() -> (String, tokio::task::JoinHandle<()>) {
    spawn(router()).await
}

pub async fn spawn_mock_sparse() -> (String, tokio::task::JoinHandle<()>) {
    spawn(sparse_router()).await
}

/// A `/v1/models` endpoint with a caller-chosen status and body, for the two error arms of
/// `list_models` that a healthy mock cannot reach: a non-2xx status and a 2xx body that is not
/// valid JSON. Only the models route is registered — these tests never complete a run.
pub async fn spawn_mock_models(status: u16, body: &'static str)
    -> (String, tokio::task::JoinHandle<()>)
{
    let code = axum::http::StatusCode::from_u16(status).expect("test status code");
    spawn(Router::new().route("/v1/models", get(move || async move { (code, body) }))).await
}

/// Binds an ephemeral port so parallel tests never collide, and returns the base URL plus the
/// server task. Dropping the task's runtime ends the mock with it.
async fn spawn(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    (format!("http://{addr}"), handle)
}

/// Deliberately takes the raw body, not `Json<Value>` and not any `Query<_>` extractor: an
/// OpenAI completion request carries everything in the body, so a `Query<_>` here would fail to
/// deserialise for a URL with no query string and every mock call would error for the wrong
/// reason. Reading the body as text also keeps the stream/BOOM checks working when the caller's
/// JSON is deliberately malformed, as the failure-path tests need.
async fn handle_completion(body: String) -> axum::response::Response {
    let streaming = body.contains("\"stream\":true");
    if body.contains("BOOM") {
        return (axum::http::StatusCode::BAD_REQUEST, "upstream exploded").into_response();
    }
    if streaming {
        // "data: " split across two chunks: the scanner must buffer, not assume boundaries.
        let a: Vec<u8> = b"data: {\"choices\":[{\"delta\":{\"content\":\"h".to_vec();
        let b: Vec<u8> = b"ello}}]}\n\n".to_vec();
        let c: Vec<u8> = b"data: [DONE]\n\n".to_vec();
        let s = futures_util::stream::iter(vec![Ok::<_, axum::Error>(a), Ok(b), Ok(c)]);
        return axum::response::Response::new(axum::body::Body::from_stream(s));
    }
    Json(json!({"choices": [{"message": {"content": "hello"}}],
                "usage": {"completion_tokens": 20, "prompt_tokens": 5, "total_tokens": 25}}))
        .into_response()
}

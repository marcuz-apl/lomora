use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::app::AppState;
use crate::error::ValidationErrors;
use crate::models::TestReq;

/// Python samples psutil/pynvml inside this handler; `hw::Sampler` owns the counters now, so
/// the handler just publishes the cached snapshot. `Snapshot::to_json` fixes the key order.
pub async fn hardware(State(st): State<AppState>) -> Json<Value> {
    Json(st.hw.snapshot().await.to_json())
}

/// Placeholder until Task 7 wires this to SQLite. Python returns oldest-first.
pub async fn get_history() -> Json<Value> {
    Json(json!([]))
}

/// Placeholder until Task 5 implements the benchmark. The extractor is load-bearing, not
/// decoration: `Result<Json<_>, _>` lets axum parse the body and hand the rejection here,
/// so a malformed body answers 422 with a `detail` array exactly like FastAPI does, while
/// `Json<TestReq>` alone would answer a plain-text 400.
pub async fn run_test_placeholder(body: Result<Json<TestReq>, JsonRejection>) -> Response {
    match body {
        Ok(Json(_req)) => Json(json!({"error": "not implemented"})).into_response(),
        // axum's JsonRejection exposes no parse offset (see ValidationErrors::json_invalid), so
        // the position is reported as 0 for now; Task 5 owns the real offset and the data-error
        // vs syntax-error split.
        Err(rejection) => ValidationErrors::json_invalid(&rejection.body_text(), 0).into_response(),
    }
}

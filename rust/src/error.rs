use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Python returns HTTP 200 with {"error": ...}; index.html reads data.error after
/// r.json(). A 4xx here would break the dashboard silently.
pub struct AppError(pub String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(json!({ "error": self.0 }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> AppError { AppError(e.to_string()) }
}

/// pydantic answers 422 with a "detail" array before the handler runs. Mirrored, not
/// "fixed" into a friendly 200, because wire parity is the goal. See spec 5.1.
pub struct ValidationErrors(pub serde_json::Value);

impl ValidationErrors {
    pub fn from_field(field: &str, msg: &str, kind: &str) -> ValidationErrors {
        ValidationErrors(json!({ "detail": [
            { "type": kind, "loc": ["body", field], "msg": msg, "input": null }
        ]}))
    }

    /// FastAPI's answer to an unparseable body: a `json_invalid` entry whose `loc[1]` is the
    /// *character offset* of the parse failure and whose `msg` is FastAPI's own decode string.
    /// axum hands back a plain-text 400 for exactly this case (it reserves 422 for well-formed
    /// JSON that does not fit the model), so handlers take `Result<Json<_>, JsonRejection>` and
    /// route the failure through here instead.
    ///
    /// `offset` is a parameter rather than a hardcoded `0` to make the call site state which
    /// position it is reporting. axum's `JsonSyntax`/`JsonData` rejections expose only
    /// `body_text()`/`status()` (the inner `serde_json::Error` line/column is `pub(crate)`), so
    /// the call site currently passes `0` and the real offset is not yet reported.
    ///
    /// This is a syntax-error envelope only. Getting the true character offset + FastAPI `msg`,
    /// and splitting a data error (a well-formed body with the wrong field type — Python reports
    /// `{"max_tokens":"x"}` on `/api/test` as `int_parsing` at `loc: ["body","max_tokens"]`, and
    /// `{"name":5}` on `POST /api/profile` as `string_type` at `loc: ["body","name"]`, with
    /// `notes` behaving the same as `name`) apart from a syntax error, are all Task 5's
    /// responsibility; do not treat this helper as the general validation shape.
    pub fn json_invalid(msg: &str, offset: usize) -> ValidationErrors {
        ValidationErrors(json!({ "detail": [
            { "type": "json_invalid", "loc": ["body", offset], "msg": "JSON decode error",
              "input": {}, "ctx": { "error": msg } }
        ]}))
    }

    /// The single mapping from axum's `JsonRejection` to FastAPI's 422 answer. Every JSON POST
    /// body goes through it — `set_profile` and `run_test_placeholder` today, and Tasks 5 and 6
    /// when their benchmark bodies arrive — rather than repeating the `json_invalid` call at each
    /// handler site. Returns the built `Response` so a handler can hand it straight back.
    ///
    /// The offset is still `0` and a well-formed-but-wrong-typed body is still reported as a
    /// syntax error; see `json_invalid` for what Task 5 owns.
    pub fn from_json_rejection(rejection: &JsonRejection) -> Response {
        ValidationErrors::json_invalid(&rejection.body_text(), 0).into_response()
    }
}

impl IntoResponse for ValidationErrors {
    fn into_response(self) -> Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(self.0)).into_response()
    }
}

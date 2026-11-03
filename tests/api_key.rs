//! The optional `api_key` on `POST /api/test` and `POST /api/stress`: forwarded upstream as
//! `Authorization: Bearer <key>` on every completion call, omitted entirely when absent or
//! blank. The rules here are the ones the dashboard needs: key present → header on both probe
//! calls and every fan-out request; key absent/blank → no header at all (LM Studio and Ollama
//! must be unaffected); a wrong-typed key is the same 422 `string_type` the other
//! `Optional[str]` fields answer.

mod support;
use support::{fresh_app, spawn_router};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Every Authorization header the upstream received, in arrival order. `None` = the request
/// carried no Authorization header at all.
type Captured = Arc<Mutex<Vec<Option<String>>>>;

/// An upstream that records each completion call's Authorization header and answers the
/// streaming TTFT probe with one data line and the full completion with a valid body — the
/// same two-call shape the real probe makes (tests/test_endpoint.rs::spawn_upstream).
async fn spawn_header_capture(captured: Captured) -> (String, tokio::task::JoinHandle<()>) {
    let app = axum::Router::new().route("/v1/chat/completions",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: String| async move {
            captured.lock().unwrap().push(
                headers.get("authorization").and_then(|v| v.to_str().ok().map(str::to_string)));
            if body.contains("\"stream\":true") {
                let frames = vec![
                    "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                    "data: [DONE]\n\n"];
                let s = futures_util::stream::iter(
                    frames.into_iter().map(|f| Ok::<_, axum::Error>(f.as_bytes().to_vec())));
                axum::response::Response::new(axum::body::Body::from_stream(s))
            } else {
                axum::Json(json!({"choices": [{"message": {"content": "hi"}}],
                                  "usage": {"completion_tokens": 20}})).into_response()
            }
        }));
    spawn_router(app).await
}

async fn post_test(base: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new().post(format!("{base}/api/test")).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

// ── the core rule: the key rides every completion call ──────────────────────

/// Both upstream calls the probe makes — the streaming TTFT probe and the full completion —
/// must carry `Authorization: Bearer <key>` when the request sets `api_key`.
#[tokio::test]
async fn api_key_reaches_the_upstream_on_both_probe_calls() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (up, _h) = spawn_header_capture(captured.clone()).await;
    let (base, _a) = fresh_app(&up).await;
    let (status, v) = post_test(&base, json!({"model":"m","base_url":up,"max_tokens":20,
                                              "api_key":"mrd_test_key"})).await;
    assert_eq!(status, 200, "{v}");
    assert!(v.get("error").is_none(), "{v}");
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 2, "the TTFT probe plus the rate call: {seen:?}");
    assert!(seen.iter().all(|h| h.as_deref() == Some("Bearer mrd_test_key")),
        "every call must carry the bearer header: {seen:?}");
}

/// The stress fan-out clones the request per task; each dispatched completion must carry the
/// header too, not just the first one.
#[tokio::test]
async fn stress_forwards_the_api_key_on_every_fan_out_request() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (up, _h) = spawn_header_capture(captured.clone()).await;
    let (base, _a) = fresh_app(&up).await;
    let r = reqwest::Client::new().post(format!("{base}/api/stress"))
        .json(&json!({"model":"m","base_url":up,"max_tokens":20,
                      "concurrency":2,"requests":3,"api_key":"mrd_stress_key"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["success"], 3, "{v}");
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen.iter().all(|h| h.as_deref() == Some("Bearer mrd_stress_key")),
        "every fan-out request must carry the bearer header: {seen:?}");
}

// ── the negative space: no key, no header ───────────────────────────────────

/// Absent `api_key` — the LM Studio / Ollama shape — must send no Authorization header, so
/// those providers are untouched by the feature.
#[tokio::test]
async fn absent_api_key_sends_no_authorization_header() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (up, _h) = spawn_header_capture(captured.clone()).await;
    let (base, _a) = fresh_app(&up).await;
    let (status, v) = post_test(&base, json!({"model":"m","base_url":up,"max_tokens":20})).await;
    assert_eq!(status, 200, "{v}");
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|h| h.is_none()), "no header may be sent: {seen:?}");
}

/// An explicit null behaves like an absent key, and a whitespace-only key counts as blank:
/// neither may produce a header (an empty `Bearer ` would be rejected by the gateway).
#[tokio::test]
async fn null_and_blank_api_keys_send_no_authorization_header() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (up, _h) = spawn_header_capture(captured.clone()).await;
    let (base, _a) = fresh_app(&up).await;
    for body in [json!({"base_url":up,"api_key":null}),
                 json!({"base_url":up,"api_key":"   "})] {
        let (status, v) = post_test(&base, body.clone()).await;
        assert_eq!(status, 200, "{body} -> {v}");
    }
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|h| h.is_none()), "no header may be sent: {seen:?}");
}

// ── validation: the key is an Optional[str] like base_url ───────────────────

/// A non-string `api_key` is a 422 `string_type` naming the field — the same rule every other
/// `Optional[str]` body field answers, not a silent ignore or a serde 400.
#[tokio::test]
async fn wrong_typed_api_key_is_a_422_string_type() {
    let (base, _h) = support::spawn_app().await;
    let (status, v) = post_test(&base, json!({"api_key": 5})).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["detail"][0], json!({"type": "string_type", "loc": ["body", "api_key"],
        "msg": "Input should be a valid string", "input": 5}), "{v}");
}

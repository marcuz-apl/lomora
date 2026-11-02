//! `POST /api/stress` — the semaphore fan-out (main.py:197-267).
//!
//! Deterministic facts (keys, order, counts, error strings, 422 shapes) are asserted exactly;
//! timings are only asserted structurally or as inequalities, because they are measured.

mod support;
use support::{fresh_app, peak_inflight, spawn_app, spawn_app_on_db, spawn_limited_mock, spawn_mock};
use serde_json::{Value, json};

async fn post(base: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new().post(format!("{base}/api/stress")).json(&body).send().await.unwrap()
}

async fn post_json(base: &str, body: Value) -> (u16, Value) {
    let r = post(base, body).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object().expect("the body must be a JSON object").keys().cloned().collect()
}

/// main.py:243-255's dict, in declaration order. The mocked upstream always answers
/// `completion_tokens: 20` (src/mock.rs), so 12 calls total 240 — the count is deterministic
/// even though the timings are not.
#[tokio::test]
async fn all_succeed_against_the_mock_and_keys_follow_python_order() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let (status, v) = post_json(&base,
        json!({"model":"mock-a","base_url":mock,"max_tokens":8,"concurrency":4,"requests":12})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(keys(&v), ["model", "base_url", "concurrency", "requests", "success", "errors",
                          "error_samples", "wall_s", "avg_latency_s", "p95_latency_s",
                          "total_tokens", "agg_tok_per_s", "req_per_s"]);
    assert_eq!(v["model"], "mock-a");
    assert_eq!(v["base_url"], mock, "the resolved base URL is echoed verbatim");
    assert_eq!(v["concurrency"], 4);
    assert_eq!(v["requests"], 12);
    assert_eq!(v["success"], 12);
    assert_eq!(v["errors"], 0);
    assert_eq!(v["error_samples"], json!([]));
    assert_eq!(v["total_tokens"], 240);
    // Measured, so only the shape and the invariant are pinned.
    assert!(v["wall_s"].as_f64().unwrap() > 0.0, "{v}");
    assert!(v["avg_latency_s"].as_f64().unwrap() > 0.0, "{v}");
    assert!(v["p95_latency_s"].as_f64().unwrap() > 0.0, "{v}");
    assert!(v["agg_tok_per_s"].as_f64().unwrap() > 0.0, "{v}");
    assert!(v["req_per_s"].as_f64().unwrap() > 0.0, "{v}");
    // The per-field rounding rules are pinned exactly in `bench::agg_tests`; here only the
    // ordering invariant holds: the 95th percentile cannot be below the mean.
    assert!(v["avg_latency_s"].as_f64().unwrap() <= v["p95_latency_s"].as_f64().unwrap(), "{v}");
}

/// Stress has no error path in Python: every failure is counted and reported under HTTP 200,
/// with the latency aggregate null (not 0) because nothing succeeded.
#[tokio::test]
async fn unreachable_upstream_returns_200_with_zero_success() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base,
        json!({"base_url":"http://127.0.0.1:9","concurrency":2,"requests":3,"max_tokens":4})).await;
    assert_eq!(status, 200, "stress has no error path");
    assert_eq!(v["success"], 0);
    assert_eq!(v["errors"], 3);
    assert!(v["avg_latency_s"].is_null() && v["p95_latency_s"].is_null(), "{v}");
    assert_eq!(v["total_tokens"], 0);
    assert_eq!(v["agg_tok_per_s"], 0.0);
    assert_eq!(v["req_per_s"], 0.0);
    let samples = v["error_samples"].as_array().unwrap();
    assert_eq!(samples.len(), 3, "all three failures, under the five-sample cap");
    assert!(samples.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty())), "{v}");
}

/// The semaphore must actually cap in-flight calls: 15 requests at concurrency 3 against a
/// 50 ms mock may never overlap more than 3, and with 15 queued calls it must overlap at least
/// 2 (otherwise this test would pass even if the fan-out ran strictly serially).
#[tokio::test]
async fn concurrency_cap_is_actually_enforced() {
    let (mock, _m) = spawn_limited_mock(50).await;
    let (base, _h) = fresh_app(&mock).await;
    let (status, v) = post_json(&base,
        json!({"base_url":mock,"concurrency":3,"requests":15,"max_tokens":8})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["success"], 15, "{v}");
    let peak = peak_inflight(&mock).await;
    assert!(peak <= 3, "the semaphore was not honoured: peak {peak}");
    assert!(peak >= 2, "the requests never overlapped, so the cap proves nothing: peak {peak}");
}

/// `Field(4, ge=1, le=32)` and `Field(20, ge=1, le=500)`, plus `max_tokens`' own bounds — all
/// checked in the shared pass, so several bad fields answer several entries in declaration
/// order. Each shape below was measured live against `:8501`.
#[tokio::test]
async fn validation_bounds_for_concurrency_requests_and_max_tokens() {
    let (base, _h) = spawn_app().await;
    for (body, kind, field) in [
        (json!({"concurrency": 40}), "less_than_equal", "concurrency"),
        (json!({"concurrency": 0}), "greater_than_equal", "concurrency"),
        (json!({"requests": 900}), "less_than_equal", "requests"),
        (json!({"requests": 0}), "greater_than_equal", "requests"),
        (json!({"max_tokens": 2049}), "less_than_equal", "max_tokens"),
    ] {
        let (status, v) = post_json(&base, body.clone()).await;
        assert_eq!(status, 422, "{body} -> {v}");
        assert_eq!(v["detail"][0]["type"], kind, "{body} -> {v}");
        assert_eq!(v["detail"][0]["loc"], json!(["body", field]), "{body} -> {v}");
        assert_eq!(v["detail"][0]["input"], body[field], "{body} -> {v}");
    }
    // Aggregated, in declaration order: max_tokens, concurrency, requests.
    let (status, v) = post_json(&base,
        json!({"max_tokens": 0, "concurrency": 99, "requests": 0})).await;
    assert_eq!(status, 422);
    let detail = v["detail"].as_array().unwrap();
    assert_eq!(detail.len(), 3, "{v}");
    assert_eq!(detail[0]["loc"], json!(["body", "max_tokens"]));
    assert_eq!(detail[1]["loc"], json!(["body", "concurrency"]));
    assert_eq!(detail[2]["loc"], json!(["body", "requests"]));
}

/// The same shared body mapping as the other JSON POSTs: a wrong-typed field is a field-level
/// 422 and a malformed body is `json_invalid`, never axum's plain-text 400.
#[tokio::test]
async fn stress_uses_the_shared_422_mapping() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base, json!({"concurrency": "x"})).await;
    assert_eq!(status, 422);
    assert_eq!(v["detail"][0]["type"], "int_parsing", "{v}");

    let r = reqwest::Client::new().post(format!("{base}/api/stress"))
        .header("Content-Type", "application/json").body("{ bad").send().await.unwrap();
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["detail"][0]["type"], "json_invalid");
    assert_eq!(v["detail"][0]["loc"], json!(["body", 2]));
}

/// Python appends the synthetic history row after the gather *unconditionally* — a run where
/// every call failed still lands in history. The row's model carries the concurrency suffix and
/// `ttft_ms` is null (main.py:259-267).
#[tokio::test]
async fn a_stress_run_is_persisted_even_when_every_call_fails() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_app_on_db(db.clone()).await;
    let (status, v) = post_json(&base,
        json!({"model":"m","base_url":"http://127.0.0.1:9","concurrency":2,"requests":3})).await;
    assert_eq!(status, 200);
    assert_eq!(v["success"], 0);
    let rows = db.recent_runs(10).await.unwrap();
    assert_eq!(rows.len(), 1, "one synthetic row, even for a wholly failed run");
    let row = &rows[0];
    assert_eq!(row.kind, "stress");
    assert_eq!(row.model, "m (stress x2)");
    assert_eq!(row.ttft_ms, None);
    assert_eq!(row.tokens, 0);
    assert_eq!(row.total_s, v["wall_s"].as_f64().unwrap());
    assert_eq!(row.tok_per_s, v["agg_tok_per_s"].as_f64().unwrap());
    assert_eq!(row.concurrency, Some(2));
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:9"));
}

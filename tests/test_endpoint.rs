//! `POST /api/test` — the probe and the shared 422 semantics.
//!
//! Timed values (`ttft_ms`, `total_s`, `tok_per_s`) are asserted structurally or against a
//! deliberately slow upstream, never by exact number: they are measured, so any absolute
//! expectation would be a race. Spec section 7.

mod support;
use support::{fresh_app, spawn_app, spawn_app_on_db, spawn_mock};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpListener;

async fn post(base: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new().post(format!("{base}/api/test")).json(&body).send().await.unwrap()
}

async fn post_json(base: &str, body: Value) -> (u16, Value) {
    let r = post(base, body).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object().expect("the body must be a JSON object").keys().cloned().collect()
}

/// A stream that carries no accepted `data: ` line, and a non-streaming completion that answers
/// after `delay_ms`. `spawn_mock`'s upstream always emits a data line and answers instantly, so
/// neither the `ttft_ms: null` path nor a meaningful `total_s` can be exercised against it.
async fn spawn_upstream(delay_ms: u64, with_data_line: bool)
    -> (String, tokio::task::JoinHandle<()>)
{
    let listener = TcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route("/v1/chat/completions",
        axum::routing::post(move |body: String| async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if body.contains("\"stream\":true") {
                let frames: Vec<&str> = if with_data_line {
                    vec!["data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                         "data: [DONE]\n\n"]
                } else {
                    // SSE comments, a named event and a [DONE]: no line the predicate accepts.
                    vec![": keep-alive\n\n", "event: message\n\n", "data: [DONE]\n\n"]
                };
                let s = futures_util::stream::iter(
                    frames.into_iter().map(|f| Ok::<_, axum::Error>(f.as_bytes().to_vec())));
                axum::response::Response::new(axum::body::Body::from_stream(s))
            } else {
                axum::Json(json!({"choices": [{"message": {"content": "hi"}}],
                                  "usage": {"completion_tokens": 20}})).into_response()
            }
        }));
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    (format!("http://{addr}"), handle)
}

/// An upstream that answers the streaming TTFT probe happily but the full completion with a 400,
/// so the `Inference failed: ...` path is reachable on its own.
async fn spawn_inference_fails() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route("/v1/chat/completions",
        axum::routing::post(|body: String| async move {
            if body.contains("\"stream\":true") {
                axum::response::Response::new(axum::body::Body::from(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"))
            } else {
                (axum::http::StatusCode::BAD_REQUEST, "model oom").into_response()
            }
        }));
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    (format!("http://{addr}"), handle)
}

// ── the brief's five ────────────────────────────────────────────────────────

#[tokio::test]
async fn success_body_has_wire_key_order_and_types() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let r = post(&base, json!({"model":"mock-a","base_url":mock,"max_tokens":20})).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(keys(&v), ["timestamp", "model", "max_tokens", "ttft_ms",
                          "total_s", "tokens", "tok_per_s"]);
    assert_eq!(v["tokens"], 20);
    assert_eq!(v["model"], "mock-a");
    assert_eq!(v["max_tokens"], 20);
    assert!(v["ttft_ms"].as_f64().unwrap() > 0.0, "{v}");
    // `total_s` is `round(elapsed, 2)`, and a local mock completes in single-digit milliseconds,
    // so this may legitimately be 0.0 — assert the number, not a floor it can't have.
    assert!(v["total_s"].is_f64(), "{v}");
    assert!(v["total_s"].as_f64().unwrap() >= 0.0, "{v}");
    assert!(v["tok_per_s"].as_f64().unwrap() > 0.0, "{v}");
    // `timestamp` is local time `%Y-%m-%d %H:%M:%S`, not RFC3339 and not an epoch. The
    // dashboard prints it verbatim in the history table, and the older time-only `%H:%M:%S`
    // shape made rows from earlier days look misordered once the table went newest-first — so
    // the stamp deliberately widens to carry the date.
    let ts = v["timestamp"].as_str().unwrap();
    chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
        .unwrap_or_else(|e| panic!("timestamp must be `%Y-%m-%d %H:%M:%S`, got {ts:?}: {e}"));
}

/// `tok_per_s` rounds `tokens / elapsed` to one place and `total_s` rounds the same `elapsed` to
/// two, so the two reported fields must agree. That is only checkable when `total_s` is big enough
/// for its own rounding not to swamp the ratio, so this runs against a ~1.1 s upstream: with
/// `elapsed ~= 1.103` the worst case is |20/1.10 - 20/1.103| ~= 0.05, inside the 0.15 tolerance.
/// Against the instant mock `total_s` quantises to 0.0 and the derived ratio is `inf`, which is
/// why the assertion belongs on a slow upstream rather than being loosened into meaninglessness.
#[tokio::test]
async fn tok_per_s_is_consistent_with_tokens_and_total_s() {
    let (up, _u) = spawn_upstream(1100, true).await;
    let (base, _h) = fresh_app(&up).await;
    let (_, v) = post_json(&base, json!({"model":"m","base_url":up,"max_tokens":20})).await;
    let total_s = v["total_s"].as_f64().unwrap();
    assert!(total_s >= 1.0, "the upstream must be slow enough to quantise, got {total_s}");
    let derived = crate_consistency(v["tokens"].as_f64().unwrap(), total_s);
    assert!((v["tok_per_s"].as_f64().unwrap() - derived).abs() < 0.15,
        "reported {} vs derived {derived}", v["tok_per_s"]);
}

fn crate_consistency(tokens: f64, total_s: f64) -> f64 { tokens / total_s }

/// The TTFT probe runs first, so its failure sets the prefix: `TTFT probe failed:`, and the
/// message carries the bare status code (`HTTP 400`), not hyper's `400 Bad Request`.
#[tokio::test]
async fn upstream_400_becomes_200_with_inference_failed_prefix() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let r = post(&base, json!({"model":"mock-a","base_url":mock,"prompt":"BOOM","max_tokens":20})).await;
    assert_eq!(r.status(), 200, "a runtime failure is never a 4xx");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"], "TTFT probe failed: HTTP 400: upstream exploded");
    // The envelope is the bare `error` key and nothing else.
    assert_eq!(keys(&v), ["error"]);
}

#[tokio::test]
async fn out_of_range_max_tokens_is_422_with_detail() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base, json!({"max_tokens": 5000})).await;
    assert_eq!(status, 422);
    assert_eq!(v["detail"][0], json!({"type": "less_than_equal", "loc": ["body", "max_tokens"],
        "msg": "Input should be less than or equal to 2048", "input": 5000,
        "ctx": {"le": 2048}}));
    // The detail entry's own key order is the contract.
    assert_eq!(keys(&v["detail"][0]), ["type", "loc", "msg", "input", "ctx"]);
}

#[tokio::test]
async fn defaults_apply_when_fields_are_omitted() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let (_, v) = post_json(&base, json!({"base_url": mock})).await;
    assert_eq!(v["model"], "local-model");
    assert_eq!(v["max_tokens"], 100);
}

// ── the shared 422 semantics ────────────────────────────────────────────────

/// `Field(100, ge=1, le=2048)` is inclusive on both ends, so 1 and 2048 must reach the probe (and
/// answer the 200 envelope against the dead default upstream) while 0 and 2049 are 422.
#[tokio::test]
async fn max_tokens_bounds_are_inclusive() {
    let (base, _h) = spawn_app().await;
    for n in [1, 2048] {
        let (status, _) = post_json(&base, json!({"max_tokens": n})).await;
        assert_eq!(status, 200, "{n} is inside the range, so validation must pass");
    }
    for (n, kind) in [(0, "greater_than_equal"), (2049, "less_than_equal")] {
        let (status, v) = post_json(&base, json!({"max_tokens": n})).await;
        assert_eq!(status, 422, "{n} -> {v}");
        assert_eq!(v["detail"][0]["type"], kind, "{n} -> {v}");
        assert_eq!(v["detail"][0]["loc"], json!(["body", "max_tokens"]));
        assert_eq!(v["detail"][0]["input"], n);
    }
}

/// A wrong-typed field is NOT a syntax error: it gets its own rule, a `loc` naming the field, and
/// the offending value echoed in `input`.
#[tokio::test]
async fn wrong_typed_field_is_a_field_error_not_json_invalid() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base, json!({"max_tokens": "x"})).await;
    assert_eq!(status, 422);
    assert_eq!(v["detail"][0], json!({"type": "int_parsing", "loc": ["body", "max_tokens"],
        "msg": "Input should be a valid integer, unable to parse string as an integer",
        "input": "x"}));
    // A type error carries no `ctx`.
    assert!(v["detail"][0].get("ctx").is_none(), "{v}");

    // The other rules the validator distinguishes: (body, rule, msg, echoed input).
    let cases = [
        (json!({"max_tokens": 1.5}), "int_from_float",
            "Input should be a valid integer, got a number with a fractional part", json!(1.5)),
        (json!({"max_tokens": null}), "int_type",
            "Input should be a valid integer", Value::Null),
        (json!({"model": 5}), "string_type", "Input should be a valid string", json!(5)),
    ];
    for (body, kind, msg, echoed) in cases {
        let (status, v) = post_json(&base, body.clone()).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(v["detail"][0]["type"], kind, "{body} -> {v}");
        assert_eq!(v["detail"][0]["msg"], msg, "{body} -> {v}");
        assert_eq!(v["detail"][0]["input"], echoed, "{body} -> {v}");
    }
}

/// Several bad fields answer one entry per field, in declaration order
/// (`model`, `prompt`, `max_tokens`, `base_url`).
#[tokio::test]
async fn several_bad_fields_report_in_declaration_order() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base,
        json!({"model": 5, "max_tokens": "x", "base_url": 7})).await;
    assert_eq!(status, 422);
    let detail = v["detail"].as_array().unwrap();
    assert_eq!(detail.len(), 3, "{v}");
    assert_eq!(detail[0]["loc"], json!(["body", "model"]));
    assert_eq!(detail[1]["loc"], json!(["body", "max_tokens"]));
    assert_eq!(detail[2]["loc"], json!(["body", "base_url"]));
    assert_eq!(detail[0]["type"], "string_type");
    assert_eq!(detail[1]["type"], "int_parsing");
    assert_eq!(detail[2]["type"], "string_type");
}

/// Types and `Field(ge/le)` bounds are checked in ONE pass, answering one 422 carrying every
/// offending field — `{"model":5,"max_tokens":5000}` returns `string_type` for `model` then
/// `less_than_equal` for `max_tokens`. A post-deserialization `validate()` could only report the
/// first, so this pins the aggregated shape end to end. The constraint entry echoes the raw
/// input: a numeric string stays a string.
#[tokio::test]
async fn type_and_range_errors_aggregate_in_one_422() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base, json!({"model": 5, "max_tokens": 5000})).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["detail"], json!([
        {"type": "string_type", "loc": ["body", "model"],
         "msg": "Input should be a valid string", "input": 5},
        {"type": "less_than_equal", "loc": ["body", "max_tokens"],
         "msg": "Input should be less than or equal to 2048", "input": 5000,
         "ctx": {"le": 2048}}]), "{v}");

    let (status, v) = post_json(&base, json!({"max_tokens": "5000"})).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["detail"][0]["type"], "less_than_equal");
    assert_eq!(v["detail"][0]["input"], "5000", "the raw string, not the coerced int");
}

/// A JSON syntax error is `json_invalid` with the character offset in `loc[1]` — a value that
/// must be computed, not hard-coded to 0.
#[tokio::test]
async fn malformed_body_is_json_invalid_at_the_character_offset() {
    let (base, _h) = spawn_app().await;
    for (raw, offset) in [("{ bad", 2), ("[1,", 3), ("{\"model\":", 9)] {
        let r = reqwest::Client::new().post(format!("{base}/api/test"))
            .header("Content-Type", "application/json").body(raw.to_string()).send()
            .await.unwrap();
        assert_eq!(r.status(), 422, "{raw}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["detail"][0]["type"], "json_invalid", "{raw} -> {v}");
        assert_eq!(v["detail"][0]["loc"], json!(["body", offset]), "{raw} -> {v}");
        assert_eq!(v["detail"][0]["msg"], "JSON decode error");
        assert_eq!(v["detail"][0]["input"], json!({}));
        // The prose inside ctx.error is the JSON parser's own; only the key is contractually
        // required.
        assert!(v["detail"][0]["ctx"]["error"].is_string(), "{v}");
    }
}

/// A body that is valid JSON but not an object never reaches the model: `model_attributes_type`
/// on `["body"]`, echoing the whole value.
#[tokio::test]
async fn non_object_body_is_model_attributes_type() {
    let (base, _h) = spawn_app().await;
    for (raw, input) in [("[1,2]", json!([1, 2])), ("\"s\"", json!("s")), ("123", json!(123))] {
        let r = reqwest::Client::new().post(format!("{base}/api/test"))
            .header("Content-Type", "application/json").body(raw.to_string()).send()
            .await.unwrap();
        assert_eq!(r.status(), 422, "{raw}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["detail"][0], json!({"type": "model_attributes_type", "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "input": input}), "{raw} -> {v}");
    }
}

/// Neither an empty nor a literal `null` body is a syntax error; both are `missing`.
#[tokio::test]
async fn empty_and_null_bodies_are_missing_not_syntax() {
    let (base, _h) = spawn_app().await;
    for raw in ["", "null"] {
        let r = reqwest::Client::new().post(format!("{base}/api/test"))
            .header("Content-Type", "application/json").body(raw.to_string()).send()
            .await.unwrap();
        assert_eq!(r.status(), 422, "{raw:?}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["detail"][0], json!({"type": "missing", "loc": ["body"],
            "msg": "Field required", "input": null}), "{raw:?} -> {v}");
    }
}

/// The same shared rules cover `POST /api/profile`: `Optional[str]` rejects a non-string but
/// accepts `null`, and its syntax errors carry the same offset rule.
#[tokio::test]
async fn profile_body_validation_shares_the_test_endpoint_semantics() {
    let (base, _h) = spawn_app().await;
    let c = reqwest::Client::new();

    // A non-string on an Optional[str] is `string_type`, echoing the value.
    let r = c.post(format!("{base}/api/profile")).json(&json!({"name": 5})).send().await.unwrap();
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["detail"][0], json!({"type": "string_type", "loc": ["body", "name"],
        "msg": "Input should be a valid string", "input": 5}));

    // The same json_invalid offset rule, on the other endpoint: `{"name": "x",,}` -> 13.
    let r = c.post(format!("{base}/api/profile"))
        .header("Content-Type", "application/json").body("{\"name\": \"x\",,}")
        .send().await.unwrap();
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["detail"][0]["type"], "json_invalid", "{v}");
    assert_eq!(v["detail"][0]["loc"], json!(["body", 13]), "{v}");

    // `null` stays legal on an Optional[str] and means "leave unchanged".
    let r = c.post(format!("{base}/api/profile")).json(&json!({"name": null}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    for (raw, kind) in [("[3]", "model_attributes_type"), ("null", "missing")] {
        let r = c.post(format!("{base}/api/profile"))
            .header("Content-Type", "application/json").body(raw.to_string()).send()
            .await.unwrap();
        assert_eq!(r.status(), 422, "{raw}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["detail"][0]["type"], kind, "{raw} -> {v}");
    }
}

/// A lax-mode value that validates must survive the type pass unchanged: `"20"` and `20.0` both
/// reach the probe as 20.
#[tokio::test]
async fn lax_numeric_coercions_are_accepted_not_rejected() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    for body in [json!({"base_url": mock, "max_tokens": "20"}),
                 json!({"base_url": mock, "max_tokens": 20.0})] {
        let (status, v) = post_json(&base, body.clone()).await;
        assert_eq!(status, 200, "{body} -> {v}");
        assert!(v.get("error").is_none(), "{v}");
        assert_eq!(v["max_tokens"], 20, "{body}");
    }
}

// ── probe behaviour ─────────────────────────────────────────────────────────

/// The result object is emitted whenever the probe finished; a stream without an accepted
/// `data: ` line leaves `ttft_ms` None, and the key stays present with a null value. It must not
/// be dropped.
#[tokio::test]
async fn ttft_ms_is_a_null_key_when_no_data_line_arrives() {
    let (up, _u) = spawn_upstream(120, false).await;
    let (base, _h) = fresh_app(&up).await;
    let r = post(&base, json!({"model":"m","base_url":up,"max_tokens":20})).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(keys(&v), ["timestamp", "model", "max_tokens", "ttft_ms",
                          "total_s", "tokens", "tok_per_s"]);
    assert!(v["ttft_ms"].is_null(), "{v}");
    assert!(v["total_s"].as_f64().unwrap() > 0.0, "{v}");
    assert_eq!(v["tokens"], 20);
}

/// The second call failing (stream fine, completion 400) is the other error prefix, with the
/// upstream body carried after the status.
#[tokio::test]
async fn inference_failure_keeps_the_ttft_probe_alive() {
    let (up, _u) = spawn_inference_fails().await;
    let (base, _h) = fresh_app(&up).await;
    let (status, v) = post_json(&base, json!({"model":"m","base_url":up,"max_tokens":20})).await;
    assert_eq!(status, 200);
    assert_eq!(v["error"], "Inference failed: HTTP 400: model oom");
    assert_eq!(keys(&v), ["error"]);
}

/// An unreachable upstream yields a connection failure: the mandated 200 `{"error": ...}`, never
/// a plain-text 400 and never a 5xx. The dead port is explicit — the default endpoint can't be
/// assumed down; a live LLM server there turns this test into a successful benchmark.
#[tokio::test]
async fn unreachable_upstream_is_a_200_error_envelope() {
    let (base, _h) = spawn_app().await;
    let (status, v) = post_json(&base, json!({"base_url": "http://127.0.0.1:9"})).await;
    assert_eq!(status, 200);
    let e = v["error"].as_str().unwrap();
    assert!(e.starts_with("TTFT probe failed: "), "{e}");
    assert_eq!(keys(&v), ["error"]);
}

/// The history endpoint reads the run back out of SQLite, so a successful run has to be a
/// `single` row carrying the same numbers the response reported — and a failed run must not be
/// recorded.
#[tokio::test]
async fn a_successful_run_is_persisted_for_the_history_endpoint() {
    let (mock, _m) = spawn_mock().await;
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_app_on_db(db.clone()).await;
    // The default base URL is dead, so the probe must be pointed at the mock explicitly.
    let (status, v) = post_json(&base, json!({"model":"mock-a","base_url":mock,"max_tokens":20})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["tokens"], 20, "{v}");
    let rows = db.recent_runs(10).await.unwrap();
    assert_eq!(rows.len(), 1, "exactly the one run");
    let row = &rows[0];
    assert_eq!(row.kind, "single");
    assert_eq!(row.model, "mock-a");
    assert_eq!(row.max_tokens, 20);
    assert_eq!(row.tokens, v["tokens"].as_i64().unwrap());
    assert_eq!(row.total_s, v["total_s"].as_f64().unwrap());
    assert_eq!(row.tok_per_s, v["tok_per_s"].as_f64().unwrap());
    assert_eq!(row.ttft_ms, v["ttft_ms"].as_f64(), "the null survives the round trip");
    assert_eq!(row.timestamp, v["timestamp"].as_str().unwrap());
    // A failed run must not be recorded.
    let (_, failed) = post_json(&base,
        json!({"model":"mock-a","base_url":mock,"prompt":"BOOM","max_tokens":20})).await;
    assert!(failed.get("error").is_some(), "{failed}");
    assert_eq!(db.recent_runs(10).await.unwrap().len(), 1);
}

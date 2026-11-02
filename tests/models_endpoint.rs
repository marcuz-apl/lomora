mod support;
use support::{spawn_app, spawn_mock};
use serde_json::Value;

// Success is a bare JSON array of id strings, because index.html branches on `Array.isArray(m)`
// to tell "list of models" from "the run failed" — array-vs-object is the contract.
#[tokio::test]
async fn returns_array_of_ids_from_the_upstream() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert!(v.is_array(), "must be an array, got {v}");
    assert_eq!(v.as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect::<Vec<_>>(),
               vec!["mock-a", "mock-b"]);
}

// This is the first route with a genuine runtime-failure path, so it is what pins the
// AppError envelope end-to-end: HTTP 200 (not 4xx/5xx), application/json, and an object whose
// only key is "error". Task 1 checked that shape against the placeholder's inline `json!`
// body; here it is checked against a real unreachable-upstream failure.
#[tokio::test]
async fn unreachable_upstream_is_200_with_error_object_not_array() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/api/models?base_url=http://127.0.0.1:9")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let v: Value = resp.json().await.unwrap();
    assert!(v.get("error").is_some(), "expected error object, got {v}");
    assert!(!v.is_array(), "a failure must never look like a model list to the frontend");
    let obj = v.as_object().expect("error envelope must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["error"],
        "the {{\"error\": ...}} envelope must carry exactly one key, got: {keys:?}");
    let msg = obj["error"].as_str().expect("error must be a string");
    assert!(!msg.is_empty(), "error message must say what failed");
}

// The sparse mock answers `{"data":[{"id":"real-one"},{"object":"model"},{"id":"","..."},
// {"id":null,...}]}`, so the filter behind Python's
// `[m.get("id") for m in data.get("data", []) if m.get("id")]` is exercised, not asserted about.
#[tokio::test]
async fn entries_without_a_usable_id_are_dropped() {
    let (mock, _m) = support::spawn_mock_sparse().await;
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap()
        .json().await.unwrap();
    assert_eq!(v.as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect::<Vec<_>>(),
               vec!["real-one"]);
}

// The non-2xx arm: Python calls `r.raise_for_status()` and answers `{"error": str(e)}`; this
// port answers the same envelope with `status.to_string()`. Only the wording of the message
// differs (httpx says "Server error '500 Internal Server Error' for url '…'"), per spec
// deviation 1 — the HTTP status, content type and single-key shape are what the UI consumes.
#[tokio::test]
async fn upstream_error_status_is_200_with_error_object() {
    let (mock, _m) = support::spawn_mock_models(500, "boom").await;
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert!(!v.is_array(), "an upstream failure must not look like a model list, got {v}");
    assert_eq!(v.as_object().map(|o| o.keys().map(String::as_str).collect::<Vec<_>>()),
               Some(vec!["error"]));
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()), "{v}");
}

// The decode-failure arm: a 2xx that is not JSON must not panic or answer an array.
#[tokio::test]
async fn undecodable_success_body_is_200_with_error_object() {
    let (mock, _m) = support::spawn_mock_models(200, "not json").await;
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert!(!v.is_array(), "a decode failure must not look like a model list, got {v}");
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()), "{v}");
}

// Documented deviation: Python's `data.get("data", [])` raises AttributeError on a non-object
// 2xx body, so it answers `{"error": …}`; this port reads `.get("data")` on any JSON value and
// answers an empty list. Pinned here so the difference is a decision, not an accident — the
// dashboard treats both as "no models" and neither crashes.
#[tokio::test]
async fn non_object_success_body_is_an_empty_list() {
    let (mock, _m) = support::spawn_mock_models(200, "[]").await;
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap()
        .json().await.unwrap();
    assert_eq!(v, Value::Array(vec![]));
}

mod support;
use support::spawn_app;
use serde_json::Value;

#[tokio::test]
async fn index_serves_the_dashboard_html() {
    let (base, _h) = spawn_app().await;
    let body = reqwest::get(format!("{base}/")).await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("<title>Lomora - LLM Monitor</title>"), "got: {body}");
    assert!(body.contains("Run Test"));
}

#[tokio::test]
async fn history_is_an_empty_array_with_status_200() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/api/history")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap(), Value::Array(vec![]));
}

#[tokio::test]
async fn malformed_json_body_is_422_not_200() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/test"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
}

// The 422 detail entry's key SEQUENCE is load-bearing: Python emits {"type","loc","msg",
// "input","ctx"} in dict-insertion order, which only survives to the wire because serde_json
// is built with `preserve_order`. A BTreeMap (feature off) re-sorts alphabetically and every
// other test still passes, so this walks detail[0] and compares the ordered key list rather
// than a parsed map. It fails the moment `preserve_order` is dropped from Cargo.toml.
#[tokio::test]
async fn validation_error_detail_keys_keep_python_order() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/test"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
    let body: Value = resp.json().await.unwrap();
    let entry = body.get("detail").and_then(Value::as_array)
        .and_then(|a| a.first()).and_then(Value::as_object)
        .expect("detail[0] must be a JSON object");
    let keys: Vec<&str> = entry.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["type", "loc", "msg", "input", "ctx"],
        "422 detail entry keys must appear in Python's dict insertion order, got: {keys:?}");
}

// A runtime failure answers HTTP 200 with a {"error": ...} object and nothing else; index.html
// reads data.error. A 4xx or an extra sibling key would break the dashboard silently.
#[tokio::test]
async fn runtime_failure_is_200_with_only_an_error_key() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/test"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m"}"#)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let obj = body.as_object().expect("runtime-failure body must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["error"],
        "the {{\"error\": ...}} envelope must carry exactly one key, got: {keys:?}");
    // The message is a live connection failure now that Task 5 replaced the placeholder; the
    // invariant is that it is a non-empty string, not a particular wording.
    assert!(obj["error"].as_str().is_some_and(|s| !s.is_empty()), "{body}");
}

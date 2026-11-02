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

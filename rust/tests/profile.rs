mod support;
use support::spawn_app;
use serde_json::{json, Value};

#[tokio::test]
async fn default_profile_name_is_my_workstation() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/profile")).await.unwrap().json().await.unwrap();
    assert_eq!(v["name"], "My workstation");
    assert_eq!(v["notes"], "");
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert!(keys.starts_with(&["name","notes","cpu","cpu_cores_physical","cpu_cores_logical","ram_total_gb","os"]), "{keys:?}");
    assert!(v["cpu_cores_logical"].as_u64().unwrap() >= v["cpu_cores_physical"].as_u64().unwrap_or(0));
}

#[tokio::test]
async fn post_updates_only_supplied_fields() {
    let (base, _h) = spawn_app().await;
    let c = reqwest::Client::new();
    let r: Value = c.post(format!("{base}/api/profile")).json(&json!({"name": "Rig A"})).send()
        .await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"name": "Rig A", "notes": ""}));
    let r: Value = c.post(format!("{base}/api/profile")).json(&json!({"notes": "rtx"})).send()
        .await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"name": "Rig A", "notes": "rtx"}));
}

#[tokio::test]
async fn empty_body_leaves_profile_untouched() {
    let (base, _h) = spawn_app().await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/api/profile")).json(&json!({"name":"Keep"})).send().await.unwrap();
    let r: Value = c.post(format!("{base}/api/profile")).json(&json!({})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["name"], "Keep");
}

mod support;
use support::{spawn_app, spawn_app_on_db};
use serde_json::{json, Value};

#[tokio::test]
async fn default_profile_name_is_my_workstation() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/profile")).await.unwrap().json().await.unwrap();
    assert_eq!(v["name"], "My workstation");
    assert_eq!(v["notes"], "");
    let obj = v.as_object().expect("profile must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    // The whole list, not `starts_with`: an exact match pins the order *and* the count, so a
    // regression that emits `"gpu": null` (forbidden — keys Python omits stay omitted) can no
    // longer pass unchanged. Same shape as `hw.rs`'s no-gpu test.
    assert_eq!(keys, vec!["name", "notes", "cpu", "cpu_cores_physical", "cpu_cores_logical",
                          "ram_total_gb", "os"],
               "the seven keys Python emits before the GPU block, in dict order");
    assert_eq!(obj.len(), 7, "nothing may be appended to the profile until Task 6 fills the GPU");
    for k in ["gpu", "gpu_vram_gb", "gpu_driver", "gpu_error"] {
        assert!(obj.get(k).is_none(), "{k} must be absent until Task 6 wires the Gpu trait");
    }
    // `cpu_cores_logical` is `available_parallelism()` (hw.rs:156), which this process can also
    // ask for, so it is pinned to that rather than to this machine's 48. The brief's machine-truth
    // check — WMI's 24 physical / 48 logical on the dev box — was done by hand and is
    // review-enforced, not test-enforced: hardcoding those numbers would fail every other box.
    let logical_expected = std::thread::available_parallelism().expect("available_parallelism")
        .get() as u64;
    let logical = v["cpu_cores_logical"].as_u64()
        .expect("cpu_cores_logical must be a number, not null or absent");
    assert_eq!(logical, logical_expected, "the API must report this process's core count");
    // Physical may legitimately be null (Python serializes `psutil.cpu_count(logical=False)`
    // even when it is None); when it is a number the invariant has to hold. No `unwrap_or(0)`
    // here — a missing or non-numeric value must fail loudly, not silently satisfy `>=`.
    match v["cpu_cores_physical"].as_u64() {
        Some(phys) => assert!(logical >= phys, "{logical} logical cores cannot be below {phys}"),
        None => assert!(v["cpu_cores_physical"].is_null(),
                        "cpu_cores_physical must be a number or null, never absent"),
    }
}

// A malformed body on this endpoint is a 422 with a `detail` array, like every other JSON POST
// (and never axum's plain-text 400). Nothing covered `POST /api/profile`'s rejection path before.
#[tokio::test]
async fn malformed_profile_body_is_422_not_200() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/profile"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("detail").and_then(Value::as_array).is_some_and(|d| !d.is_empty()),
            "a rejection must answer FastAPI's detail array, got {body}");
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

// A failed `UPDATE profile` is a runtime failure, so it must answer HTTP 200 with exactly
// `{"error": "<message>"}` — the same envelope `AppError` exists for, and the first live
// consumer of it. The bug this pins: swallowing the write error made the handler read the
// stale row back and answer `{"name": ..., "notes": ...}`, i.e. a success-shaped body for a
// lost save (`static/index.html:329` prints "Profile saved" whatever comes back).
#[tokio::test]
async fn profile_write_failure_is_200_error_envelope() {
    let (base, _h) = spawn_app_on_db(lomora::store::Db::profile_writes_fail_for_tests()).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/profile"))
        .json(&json!({"name": "Rig A"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "a runtime failure must never be a 4xx/5xx");
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: Value = resp.json().await.unwrap();
    let obj = body.as_object().expect("runtime-failure body must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["error"],
        "a failed save must not answer with the stale profile, got: {keys:?}");
    let msg = obj["error"].as_str().expect("error must be a string");
    assert!(!msg.trim().is_empty(), "the error message must say what failed");
}

// The read side of the same rule. Masking a failed read as the factory profile was the worse
// half of the bug: the UI loads the answer into the form, so the next Save writes
// "My workstation" over the user's real profile. A success-shaped body here is that bug.
#[tokio::test]
async fn profile_read_failure_is_200_error_envelope_not_the_default_profile() {
    let (base, _h) = spawn_app_on_db(lomora::store::Db::unreadable_for_tests()).await;
    let resp = reqwest::get(format!("{base}/api/profile")).await.unwrap();
    assert_eq!(resp.status(), 200, "a runtime failure must never be a 4xx/5xx");
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: Value = resp.json().await.unwrap();
    let obj = body.as_object().expect("runtime-failure body must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["error"], "a failed read must not answer the factory profile, got: {body}");
    assert!(obj["error"].as_str().is_some_and(|s| !s.trim().is_empty()), "{body}");
}

mod support;
use support::{fresh_app, fresh_app_on_db, spawn_app_on_db, spawn_mock};
use serde_json::{json, Value};

/// Python keeps a `deque(maxlen=60)` of dicts, appends newest, and `list(history)` hands the
/// dashboard oldest-first (index.html reverses it back for display). Our rows live in SQLite,
/// so `ORDER BY id DESC LIMIT 60` + `reverse()` must reproduce that order *and* the seven-key
/// dict, in dict insertion order.
#[tokio::test]
async fn rows_appear_oldest_first_with_seven_python_keys() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let c = reqwest::Client::new();
    for i in 0..3 {
        c.post(format!("{base}/api/test"))
            .json(&json!({"model": format!("m{i}"), "base_url": mock, "max_tokens": 8}))
            .send().await.unwrap();
    }
    let v: Value = c.get(format!("{base}/api/history")).send().await.unwrap().json().await.unwrap();
    let arr = v.as_array().expect("history must be a JSON array");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["model"], "m0", "oldest first");
    assert_eq!(arr[1]["model"], "m1");
    assert_eq!(arr[2]["model"], "m2");
    let obj = arr[0].as_object().expect("a history row must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, ["timestamp", "model", "max_tokens", "ttft_ms", "total_s", "tokens", "tok_per_s"],
               "exactly Python's seven keys, in dict insertion order");
    assert!(obj["timestamp"].as_str().is_some_and(|s| s.len() == 8 && s.as_bytes()[2] == b':'),
            "timestamp must be HH:MM:SS like Python's strftime, got {:?}", obj["timestamp"]);
}

/// The deque drops the *oldest* entry, so after 65 runs the five earliest are gone and the last
/// sixty, still oldest-first, survive. Distinct model names make a wrong end of the window
/// visible; an identical name would pass under either direction.
#[tokio::test]
async fn history_is_capped_at_the_sixty_most_recent_oldest_first() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let c = reqwest::Client::new();
    for i in 0..65 {
        c.post(format!("{base}/api/test"))
            .json(&json!({"model": format!("m{i}"), "base_url": mock, "max_tokens": 4}))
            .send().await.unwrap();
    }
    let v: Value = c.get(format!("{base}/api/history")).send().await.unwrap().json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 60, "the deque maxlen is 60");
    assert_eq!(arr[0]["model"], "m5", "m0..m4 fell off the far end");
    assert_eq!(arr[59]["model"], "m64", "the newest row is last, not first");
}

/// A stress run is one synthetic row: the model carries the concurrency suffix, and ttft is
/// null — Python never measures a single TTFT for a fan-out.
#[tokio::test]
async fn stress_rows_show_up_as_synthetic_entries() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/api/stress"))
        .json(&json!({"base_url": mock, "model": "m", "requests": 2, "concurrency": 2,
                      "max_tokens": 4}))
        .send().await.unwrap();
    let v: Value = c.get(format!("{base}/api/history")).send().await.unwrap().json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1, "one stress run is one row, not one row per request");
    let row = &arr[0];
    assert_eq!(row["model"], "m (stress x2)");
    assert!(row["ttft_ms"].is_null(), "stress rows answer null ttft, with the key present");
    assert_eq!(row["max_tokens"], 4);
}

/// The whole point of SQLite: a run outlives the process that recorded it. Two apps over one
/// file, with the first fully stopped before the second opens it, is the restart under test —
/// Python's `deque` starts empty again, spec deviation 1.
#[tokio::test]
async fn rows_survive_a_restart_on_a_shared_db_path() {
    let (mock, _m) = spawn_mock().await;
    let dir = std::env::temp_dir().join(format!("lomora-history-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("h.db");
    {
        let (base, h) = fresh_app_on_db(&mock, &db).await;
        reqwest::Client::new().post(format!("{base}/api/test"))
            .json(&json!({"model": "kept", "base_url": mock, "max_tokens": 8}))
            .send().await.unwrap().text().await.unwrap();
        // Dropping a JoinHandle detaches the task rather than stopping it, and its `Db` would
        // keep the file open for the "restarted" app. Abort and await it to model a real exit.
        h.abort();
        assert!(h.await.unwrap_err().is_cancelled());
    }
    let (base2, _h2) = fresh_app_on_db(&mock, &db).await;
    let v: Value = reqwest::Client::new().get(format!("{base2}/api/history"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "run must survive restart");
    assert_eq!(v.as_array().unwrap()[0]["model"], "kept");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of "empty is not failure": an unavailable `runs` table must answer HTTP 200
/// with only `{"error": …}`, the same envelope `GET /api/profile` uses. Laundering it into `[]`
/// would make a broken database look like a fresh install in the dashboard.
#[tokio::test]
async fn failed_history_read_is_a_200_error_envelope_not_an_empty_list() {
    let (base, _h) = spawn_app_on_db(lomora::store::Db::unreadable_for_tests()).await;
    let resp = reqwest::get(format!("{base}/api/history")).await.unwrap();
    assert_eq!(resp.status(), 200, "a runtime failure must never be a 4xx/5xx");
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: Value = resp.json().await.unwrap();
    let obj = body.as_object().expect("runtime-failure body must be a JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["error"], "a failed read must not answer an empty list, got: {body}");
    assert!(obj["error"].as_str().is_some_and(|s| !s.trim().is_empty()), "{body}");
}

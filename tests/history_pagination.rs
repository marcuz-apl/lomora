//! `GET /api/history` paging: the dashboard's pager fetches `?limit=10&offset=N` windows plus
//! an `X-Total-Runs` header, so it can page the *whole* dataset instead of only the last 60
//! rows. The body shape (JSON array, oldest-first within the page) is the same as ever, and a
//! request without params must stay byte-for-byte the old 60-row answer.

mod support;

use lomora::store::RunRow;
use serde_json::Value;
use support::spawn_app_on_db;

/// A minimal single-run row; only `model` varies, and distinct names make any wrong window
/// boundary visible instead of passing under identical rows.
fn row(model: &str) -> RunRow {
    RunRow {
        id: 0,
        kind: "single".into(),
        timestamp: "09:00:01".into(),
        model: model.into(),
        max_tokens: 8,
        ttft_ms: None,
        total_s: 1.0,
        tokens: 8,
        tok_per_s: 8.0,
        concurrency: None,
        base_url: None,
        provider: None,
    }
}

/// Seeds `n` rows named m00..m{n-1} directly through the store: 65 POSTs would exercise the
/// same insert path at a much slower rate, and the seeding itself is not what's under test.
async fn seed(db: &lomora::store::Db, n: usize) {
    for i in 0..n {
        db.insert_run(&row(&format!("m{i:02}"))).await.unwrap();
    }
}

#[tokio::test]
async fn history_pages_through_the_dataset_ten_rows_at_a_time() {
    let db = lomora::store::Db::in_memory();
    seed(&db, 25).await;
    let (base, h) = spawn_app_on_db(db).await;
    let c = reqwest::Client::new();

    // Page 1: the newest ten rows, oldest-first within the page (same order rule as always).
    let r = c.get(format!("{base}/api/history?limit=10&offset=0")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let total = r.headers().get("x-total-runs").expect("total header").to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    let arr = v.as_array().expect("history must stay a JSON array");
    assert_eq!(arr.len(), 10, "{v}");
    assert_eq!(arr[0]["model"], "m15", "newest window, oldest first within it");
    assert_eq!(arr[9]["model"], "m24");
    assert_eq!(total, "25", "the pager needs the unfiltered total to know the page count");

    // Page 2 and the short last page: windows slice the same stream without overlap.
    let r = c.get(format!("{base}/api/history?limit=10&offset=10")).send().await.unwrap();
    let total = r.headers().get("x-total-runs").expect("total header").to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 10);
    assert_eq!(arr[0]["model"], "m05", "{v}");
    assert_eq!(arr[9]["model"], "m14");
    assert_eq!(total, "25");

    let r = c.get(format!("{base}/api/history?limit=10&offset=20")).send().await.unwrap();
    let total = r.headers().get("x-total-runs").expect("total header").to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 5, "the tail page carries only what is left");
    assert_eq!(arr[0]["model"], "m00", "{v}");
    assert_eq!(total, "25");
    h.abort();
}

/// The default response is a stable wire surface: no params must behave exactly as before —
/// 60 newest rows, oldest-first — now merely accompanied by the total header.
#[tokio::test]
async fn history_without_params_stays_the_sixty_row_default() {
    let db = lomora::store::Db::in_memory();
    seed(&db, 65).await;
    let (base, h) = spawn_app_on_db(db).await;
    let r = reqwest::get(format!("{base}/api/history")).await.unwrap();
    let total = r.headers().get("x-total-runs").expect("total header").to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 60, "the no-param default is still the newest-sixty window");
    assert_eq!(arr[0]["model"], "m05", "m00..m04 fell off the far end");
    assert_eq!(arr[59]["model"], "m64", "newest last, oldest first");
    assert_eq!(total, "65");
    h.abort();
}

/// Pager bounds: an offset past the end is an empty page with the true total (not an error),
/// and a negative offset is clamped to the first page rather than wrapping SQLite's OFFSET
/// into something surprising.
#[tokio::test]
async fn history_pager_bounds_are_sanitized() {
    let db = lomora::store::Db::in_memory();
    seed(&db, 25).await;
    let (base, h) = spawn_app_on_db(db).await;
    let c = reqwest::Client::new();

    let r = c.get(format!("{base}/api/history?limit=10&offset=1000")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let total = r.headers().get("x-total-runs").expect("total header").to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 0, "past the end there is simply nothing");
    assert_eq!(total, "25");

    let r = c.get(format!("{base}/api/history?limit=10&offset=-5")).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 10, "a negative offset reads as the first page");
    assert_eq!(v.as_array().unwrap()[0]["model"], "m15", "{v}");

    // A page size of zero would turn every page into an empty one and the pager into a
    // dead end; it is clamped to a one-row page instead.
    let r = c.get(format!("{base}/api/history?limit=0")).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    h.abort();
}

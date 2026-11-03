//! The prompt/question pool: `GET /api/prompts` serves the built-in pool merged with whatever
//! `POST /api/prompts/fetch` has cached in SQLite, and the fetcher samples a few questions from
//! every configured benchmark source. The three built-in sources ship three different wire
//! formats (GSM8K JSONL `question`, HumanEval gzipped JSONL `prompt`, TruthfulQA JSON array of
//! `question` objects) — the mock below serves the same three shapes plus a free-form
//! `[{category, text}]` array, so one test pins every parser through the real endpoint.

mod support;

use serde_json::Value;
use support::spawn_app;

fn temp_db_path(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir()
        .join(format!("lomora-prompts-{}-{tag}.db", std::process::id()));
    let _ = std::fs::remove_file(&path); // a crashed earlier run must not leak rows
    path
}

/// gzip for the HumanEval-shaped source: the fetcher must sniff the magic bytes itself.
fn gz(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

/// One mock upstream speaking all four accepted formats at once, 3 items each (the custom one
/// carries 2). Keeping every source at or under the per-source sample cap makes the fetch
/// deterministic: the sampler must hand back the whole set, never a random subset.
fn multi_source_router() -> axum::Router {
    use axum::routing::get;
    let math = concat!(
        r#"{"question": "q1", "answer": "a1"}"#, "\n",
        r#"{"question": "q2", "answer": "a2"}"#, "\n",
        r#"{ not json }"#, "\n",               // a broken line is skipped, not fatal
        r#"{"question": "q3", "answer": "a3"}"#, "\n",
        "\n"                                   // a blank line is skipped too
    );
    let coding = gz(concat!(
        r#"{"task_id": "HumanEval/0", "prompt": "c1"}"#, "\n",
        r#"{"task_id": "HumanEval/1", "prompt": "c2"}"#, "\n",
        r#"{"task_id": "HumanEval/2", "prompt": "c3"}"#, "\n",
    ).as_bytes());
    let knowledge = r#"[{"question": "k1", "mc1_targets": {}}, {"question": "k2"}, {"question": "k3"}]"#;
    let custom = r#"[{"category": "trivia", "text": "t1"}, {"text": "t2"}]"#;
    axum::Router::new()
        .route("/math.jsonl", get(move || async move { math }))
        .route("/coding.jsonl.gz", get(move || async move { coding.clone() }))
        .route("/knowledge.json", get(move || async move { knowledge }))
        .route("/custom.json", get(move || async move { custom }))
}

fn cfg_with_sources(sources: Vec<(String, String)>) -> lomora::config::Config {
    let mut cfg = lomora::config::Config::from_env();
    cfg.prompts_sources = sources;
    cfg
}

async fn get_pool(base: &str) -> Value {
    reqwest::get(format!("{base}/api/prompts")).await.unwrap().json().await.unwrap()
}

fn texts_of(pool: &[Value]) -> Vec<&str> {
    pool.iter().map(|q| q["text"].as_str().expect("every pool entry has text")).collect()
}

// ── GET /api/prompts ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_prompts_lists_three_defaults_and_a_categorised_pool() {
    let (base, h) = spawn_app().await;
    let v = get_pool(&base).await;

    let defaults = v["defaults"].as_array().expect("defaults is an array");
    assert_eq!(defaults.len(), 3, "exactly three defaults: {v}");
    assert!(defaults.iter().all(|d| d.is_string() && !d.as_str().unwrap().is_empty()),
            "every default is a non-empty string");

    let pool = v["pool"].as_array().expect("pool is an array");
    assert!(!pool.is_empty(), "the built-in pool ships questions");
    for q in pool {
        assert!(q["category"].is_string() && !q["category"].as_str().unwrap().is_empty(),
                "every question is categorised: {q}");
        assert!(q["text"].is_string() && !q["text"].as_str().unwrap().is_empty(), "{q}");
    }
    let texts = texts_of(pool);
    for d in defaults {
        assert!(texts.contains(&d.as_str().unwrap()),
                "each default must be pickable from the pool: {d}");
    }
    h.abort();
}

#[tokio::test]
async fn an_unreadable_database_answers_the_200_error_envelope_not_a_default_pool() {
    // Same rule the profile and history endpoints follow: a store failure must not masquerade
    // as "the pool is just the built-in one" — the UI would hide a broken database.
    let (base, h) = support::spawn_app_on_db(lomora::store::Db::unreadable_for_tests()).await;
    let r = reqwest::get(format!("{base}/api/prompts")).await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"].as_str().unwrap_or_default().contains("no such table"), "{v}");
    h.abort();
}

// ── POST /api/prompts/fetch ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_imports_every_source_format_and_the_cache_survives_a_restart() {
    let (mock, _mh) = support::spawn_router(multi_source_router()).await;
    let sources = vec![
        ("math".to_string(), format!("{mock}/math.jsonl")),
        ("coding".to_string(), format!("{mock}/coding.jsonl.gz")),
        ("knowledge".to_string(), format!("{mock}/knowledge.json")),
        ("custom".to_string(), format!("{mock}/custom.json")),
    ];
    let path = temp_db_path("fetch-restart");

    let (base, h) = support::fresh_app_cfg_on_db(cfg_with_sources(sources.clone()), &path).await;
    let r = reqwest::Client::new()
        .post(format!("{base}/api/prompts/fetch")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let d: Value = r.json().await.unwrap();
    assert!(d.get("error").is_none(), "fetch must succeed: {d}");
    // 3 math + 3 coding + 3 knowledge + 2 custom = 11; the broken and blank lines never count.
    assert_eq!(d["added"].as_u64().unwrap(), 11, "{d}");

    let v = get_pool(&base).await;
    let pool = v["pool"].as_array().unwrap();
    let find = |t: &str| pool.iter()
        .find(|q| q["text"].as_str() == Some(t))
        .unwrap_or_else(|| panic!("{t} missing from the pool after fetch"));
    assert_eq!(find("q2")["category"], "math", "GSM8K-style `question` lines");
    assert_eq!(find("c2")["category"], "coding", "gzipped HumanEval-style `prompt` lines");
    assert_eq!(find("k2")["category"], "knowledge", "TruthfulQA-style JSON array");
    // A custom array may tag its own categories; the source's category is the fallback.
    assert_eq!(find("t1")["category"], "trivia");
    assert_eq!(find("t2")["category"], "custom");
    h.abort(); assert!(h.await.unwrap_err().is_cancelled());

    // The cache lives in SQLite, so a fresh process over the same file still has the questions.
    let (base2, h2) = support::fresh_app_cfg_on_db(cfg_with_sources(sources), &path).await;
    let v2 = get_pool(&base2).await;
    assert!(texts_of(v2["pool"].as_array().unwrap()).contains(&"q2"),
            "cached questions survive a restart: {v2}");
    h2.abort(); let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_second_fetch_replaces_the_cache_instead_of_appending() {
    let (mock, _mh) = support::spawn_router(multi_source_router()).await;
    let sources = vec![("math".to_string(), format!("{mock}/math.jsonl"))];
    let (base, h) = support::fresh_app_cfg_on_db(
        cfg_with_sources(sources), &temp_db_path("replace")).await;
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let d: Value = client.post(format!("{base}/api/prompts/fetch")).send().await
            .unwrap().json().await.unwrap();
        assert!(d.get("error").is_none(), "{d}");
    }
    let v = get_pool(&base).await;
    let q2 = texts_of(v["pool"].as_array().unwrap()).iter().filter(|t| **t == "q2").count();
    assert_eq!(q2, 1, "re-fetching must replace the cached set, not duplicate it: {v}");
    h.abort();
}

#[tokio::test]
async fn fetch_with_every_source_failing_answers_200_with_an_error_and_keeps_the_pool() {
    let sources = vec![
        ("math".to_string(), "http://127.0.0.1:9/math.jsonl".to_string()),
        ("coding".to_string(), "http://127.0.0.1:9/coding.jsonl.gz".to_string()),
    ];
    let (base, h) = spawn_app_cfg(sources).await;
    let r = reqwest::Client::new()
        .post(format!("{base}/api/prompts/fetch")).send().await.unwrap();
    assert_eq!(r.status(), 200, "fetch failures follow the app's 200-envelope rule");
    let d: Value = r.json().await.unwrap();
    let err = d["error"].as_str().expect("an error object: {d}");
    assert!(err.contains("question fetch failed"), "{d}");

    // And the UI falls back to exactly what it had: the built-in pool, untouched.
    let v = get_pool(&base).await;
    assert!(v["pool"].as_array().unwrap().len() >= 3, "{v}");
    assert_eq!(v["defaults"].as_array().unwrap().len(), 3, "{v}");
    h.abort();
}

#[tokio::test]
async fn a_partial_failure_imports_the_healthy_sources_and_names_the_dead_one() {
    let (mock, _mh) = support::spawn_router(multi_source_router()).await;
    let sources = vec![
        ("math".to_string(), format!("{mock}/math.jsonl")),
        ("coding".to_string(), "http://127.0.0.1:9/coding.jsonl.gz".to_string()),
    ];
    let (base, h) = spawn_app_cfg(sources).await;
    let d: Value = reqwest::Client::new()
        .post(format!("{base}/api/prompts/fetch")).send().await.unwrap().json().await.unwrap();
    assert!(d.get("error").is_none(), "one healthy source is enough: {d}");
    assert_eq!(d["added"].as_u64().unwrap(), 3, "{d}");
    let failed = d["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{d}");
    assert_eq!(failed[0]["category"], "coding");
    assert!(!failed[0]["error"].as_str().unwrap().is_empty(), "{d}");
    h.abort();
}

// helper kept at the bottom: the sources-only variant of the config constructor above
async fn spawn_app_cfg(sources: Vec<(String, String)>)
    -> (String, tokio::task::JoinHandle<()>)
{
    support::fresh_app_cfg(cfg_with_sources(sources)).await
}

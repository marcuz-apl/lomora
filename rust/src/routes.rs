use std::time::{Duration, Instant};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::bench::{SseScanner, round_half_even};
use crate::config::{Config, resolve_base};
use crate::error::{AppError, ValidationErrors};
use crate::models::{ProfileOut, ProfileReq, TestOut, TestReq};
use crate::store::RunRow;

/// `base_url: Optional[str] = None` from main.py: absent, empty, or whitespace-only all fall
/// back to the configured LM Studio URL, so `#[serde(default)]` on an `Option` is the shape.
#[derive(Deserialize)]
pub struct ModelQuery {
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Python's `list_models`: one GET `{base}/v1/models` with a 10 s timeout, and *any* failure
/// answers HTTP 200 with `{"error": ...}` — never a `Result`, because the frontend's
/// `Array.isArray(m)` check is what separates a model list from a failure. A success answers a
/// bare array of id strings, dropping entries whose `id` is missing, empty, or not a string,
/// which is Python's truthiness filter `if m.get("id")`.
///
/// The client is `AppState.http`, built once in `main.rs`, not a new client per request the way
/// `httpx.AsyncClient` is constructed inside the Python handler; the per-request `.timeout(10s)`
/// still matches Python's `timeout=10`.
pub async fn list_models(State(st): State<AppState>, Query(q): Query<ModelQuery>) -> Json<Value> {
    let base = resolve_base(&st.cfg, q.base_url.as_deref());
    match st.http.get(format!("{base}/v1/models")).timeout(std::time::Duration::from_secs(10))
        .send().await
    {
        Err(e) => Json(json!({ "error": e.to_string() })),
        Ok(r) => {
            let status = r.status();
            if !status.is_success() {
                return Json(json!({ "error": status.to_string() }));
            }
            match r.json::<Value>().await {
                Err(e) => Json(json!({ "error": e.to_string() })),
                Ok(body) => {
                    let ids: Vec<Value> = body.get("data").and_then(|d| d.as_array())
                        .map(|arr| arr.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str())
                            .filter(|s| !s.is_empty()).map(|s| Value::String(s.to_string())))
                            .collect())
                        .unwrap_or_default();
                    Json(Value::Array(ids))
                }
            }
        }
    }
}

/// Python samples psutil/pynvml inside this handler; `hw::Sampler` owns the counters now, so
/// the handler just publishes the cached snapshot. `Snapshot::to_json` fixes the key order.
pub async fn hardware(State(st): State<AppState>) -> Json<Value> {
    Json(st.hw.snapshot().await.to_json())
}

/// Placeholder until Task 7 wires this to SQLite. Python returns oldest-first.
pub async fn get_history() -> Json<Value> {
    Json(json!([]))
}

/// Python's `get_profile` (main.py:160). The mutable part (`name`, `notes`) comes from the
/// SQLite `profile` row; the immutable part from `ProfileStatic`, captured once at startup —
/// rebuilding system info per request is the per-call `psutil` cost this port removes
/// (ruling R4). `ram_total_gb` rides the `Sampler`'s cached snapshot instead.
/// The `gpu*` fields stay `None` (absent keys) until Task 6 wires the `Gpu` trait.
/// `ProfileOut::to_json` fixes the key order to Python's dict order (main.py:163).
/// A failed read is a runtime failure, so it answers the mandated 200 `{"error": …}` through
/// `AppError` rather than the factory profile: the old `.unwrap_or(default)` silently turned a
/// busy database into a "My workstation" the next Save would write back over the real row.
pub async fn get_profile(State(st): State<AppState>) -> Result<Json<Value>, AppError> {
    let p = st.db.profile().await?;
    let s = st.hw.snapshot().await;
    let sys = &st.sys;   // the startup-captured machine facts; see the note above
    let out = ProfileOut {
        name: p.name, notes: p.notes, cpu: sys.cpu.clone(),
        cpu_cores_physical: sys.cores_physical,
        cpu_cores_logical: sys.cores_logical,
        ram_total_gb: s.ram_total_gb, os: sys.os.clone(), gpu: None, gpu_vram_gb: None,
        gpu_driver: None, gpu_error: None,
    };
    Ok(Json(out.to_json()))
}

/// Python's `set_profile` (main.py:189): absent or `null` leaves the stored value unchanged,
/// present (including `""`) overwrites; the reply is the bare `{"name", "notes"}` dict.
///
/// The body goes through the one shared mapping, `ValidationErrors::parse_json_body` — a
/// malformed body is FastAPI's 422 `{"detail": [ ... ]}` (never axum's plain-text 400), and a
/// wrong-typed `name`/`notes` is the `string_type` entry captured live from `:8501`.
///
/// The `Err` arm is `AppError`, not a swallowed write: a failed `UPDATE profile` must answer the
/// mandated 200 `{"error": …}`, and must never answer the stale row, because
/// `static/index.html:329` prints "Profile saved" whatever the body says.
pub async fn set_profile(State(st): State<AppState>, body: Bytes) -> Result<Response, AppError> {
    let req = match ValidationErrors::parse_json_body::<ProfileReq>(&body, ProfileReq::FIELDS) {
        Ok(req) => req,
        Err(v) => return Ok(v.into_response()),
    };
    let p = st.db.set_profile(req.name.as_deref(), req.notes.as_deref()).await?;
    Ok(Json(json!({ "name": p.name, "notes": p.notes })).into_response())
}

/// Python's `run_test` (main.py:78). Validation (422) happens before the probe, exactly like
/// FastAPI validates the model before the handler body runs; a runtime failure is the mandated
/// 200 `{"error": …}` through `AppError`; a success is persisted through `Db::insert_run` so
/// Task 7's history sees it — the DB write failing is itself a runtime failure, answered
/// through `AppError` rather than swallowed (the Task 4 review ruling: a lost write must not
/// look like a success).
pub async fn run_test(State(st): State<AppState>, body: Bytes) -> Response {
    // Type AND `max_tokens` bounds are checked by the one shared pass, so a 422 carries every
    // offending field exactly as pydantic's does (see `TestReq::FIELDS`).
    let req = match ValidationErrors::parse_json_body::<TestReq>(&body, TestReq::FIELDS) {
        Ok(req) => req,
        Err(v) => return v.into_response(),
    };
    match probe(&req, &st.cfg, &st.http).await {
        Ok((out, row)) => {
            if let Err(e) = st.db.insert_run(&row).await { return AppError::from(e).into_response(); }
            Json(out).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// The whole probe against `base`, no Axum types: the same two upstream calls as Python —
/// a streaming one-token TTFT probe (60 s) and a full non-streaming completion (300 s) whose
/// `usage.completion_tokens / elapsed` is the token rate. Returns the wire value and the row
/// to persist. The client is the shared `AppState.http`; the per-request `.timeout()` calls
/// override the builder's 300 s ceiling exactly like Python's per-call `httpx` clients.
async fn probe(req: &TestReq, cfg: &Config, http: &reqwest::Client)
    -> Result<(Value, RunRow), AppError>
{
    let base = resolve_base(cfg, req.base_url.as_deref());

    // 1) TTFT: stream ONE token — main.py:93 sends `"max_tokens": 1`, not `req.max_tokens`;
    //    streaming the full budget would measure total time, not first-token time.
    let payload = json!({
        "model": req.model, "messages": [{"role": "user", "content": req.prompt}],
        "max_tokens": 1, "stream": true });
    let t0 = Instant::now();
    let r = http.post(format!("{base}/v1/chat/completions"))
        .timeout(Duration::from_secs(60)).json(&payload).send().await
        .map_err(|e| AppError(format!("TTFT probe failed: {e}")))?;
    if r.status().as_u16() >= 400 {
        // Python formats `HTTP {r.status_code}` — the bare number, not hyper's "400 Bad Request".
        let code = r.status().as_u16();
        let body = r.text().await.unwrap_or_default();
        return Err(AppError(format!("TTFT probe failed: HTTP {code}: {body}")));
    }
    let mut scanner = SseScanner::default();
    let mut stream = r.bytes_stream();
    let ttft_ms = loop {
        match stream.next().await {
            Some(Ok(chunk)) => if scanner.push(&chunk) {
                break Some(t0.elapsed().as_secs_f64() * 1000.0);
            },
            Some(Err(e)) => return Err(AppError(format!("TTFT probe failed: {e}"))),
            None => break None,
        }
    };

    // 2) Token rate: non-streaming, full max_tokens. The body is consumed before `elapsed` is
    //    taken because httpx's `post()` returns only after the response body is read; timing
    //    it at the headers would drop body-transfer time Python counts.
    let t1 = Instant::now();
    let payload2 = json!({
        "model": req.model, "messages": [{"role": "user", "content": req.prompt}],
        "max_tokens": req.max_tokens });
    let r2 = http.post(format!("{base}/v1/chat/completions"))
        .timeout(Duration::from_secs(300)).json(&payload2).send().await
        .map_err(|e| AppError(format!("Inference failed: {e}")))?;
    let status = r2.status();
    let text = r2.text().await.map_err(|e| AppError(format!("Inference failed: {e}")))?;
    let elapsed = t1.elapsed().as_secs_f64();
    if status.as_u16() >= 400 {
        return Err(AppError(format!("Inference failed: HTTP {}: {text}", status.as_u16())));
    }
    let body: Value = serde_json::from_str(&text)
        .map_err(|e| AppError(format!("Inference failed: invalid JSON: {e}")))?;
    let tokens = body.get("usage").and_then(|u| u.get("completion_tokens"))
        .and_then(|c| c.as_i64()).unwrap_or(0);
    let tps = if elapsed > 0.0 { round_half_even(tokens as f64 / elapsed, 1) } else { 0.0 };

    // Python's truthiness: `"ttft_ms": round(ttft_ms, 1) if ttft_ms else None` — the *unrounded*
    // value decides, and a falsy one emits a null key (the key is never dropped; a probe that
    // raised never reaches this struct at all).
    let out = TestOut {
        timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        model: req.model.clone(), max_tokens: req.max_tokens,
        ttft_ms: ttft_ms.filter(|v| *v != 0.0).map(|v| round_half_even(v, 1)),
        total_s: round_half_even(elapsed, 2), tokens, tok_per_s: tps };
    let row = RunRow {
        kind: "single".into(), timestamp: out.timestamp.clone(), model: out.model.clone(),
        max_tokens: out.max_tokens, ttft_ms: out.ttft_ms, total_s: out.total_s,
        tokens: out.tokens, tok_per_s: out.tok_per_s, concurrency: None, base_url: None };
    let value = serde_json::to_value(&out)
        .map_err(|e| AppError(format!("failed to serialize the test result: {e}")))?;
    Ok((value, row))
}

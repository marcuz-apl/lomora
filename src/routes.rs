use std::sync::Arc;
use std::time::{Duration, Instant};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::app::AppState;
use crate::bench::{SseScanner, from_samples, round_half_even};
use crate::config::{Config, resolve_base};
use crate::error::{AppError, ValidationErrors};
use crate::models::{ProfileOut, ProfileReq, StressOut, StressReq, TestOut, TestReq};
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

/// Python's `deque(maxlen=60)` handed back as `list(history)`: oldest-first, at most sixty
/// entries. A failed read is a runtime failure, so it answers the mandated 200 `{"error": …}`
/// through `AppError` instead of `unwrap_or_default()` — an empty list is a legitimate answer
/// ("no runs yet"), and it must not be indistinguishable from "the database is unavailable".
pub async fn get_history(State(st): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = st.db.recent_runs(60).await?;
    Ok(Json(Value::Array(rows.iter().map(|r| r.to_history_json()).collect())))
}

/// Python's `get_profile` (main.py:160). The mutable part (`name`, `notes`) comes from the
/// SQLite `profile` row; the immutable part from `ProfileStatic`, captured once at startup —
/// rebuilding system info per request is the per-call `psutil` cost this port removes
/// (ruling R4). `ram_total_gb` rides the `Sampler`'s cached snapshot instead, and the `gpu*`
/// fields come from the handle the sampler already samples.
/// `ProfileOut::to_json` fixes the key order to Python's dict order (main.py:163).
/// A failed read is a runtime failure, so it answers the mandated 200 `{"error": …}` through
/// `AppError` rather than the factory profile: the old `.unwrap_or(default)` silently turned a
/// busy database into a "My workstation" the next Save would write back over the real row.
pub async fn get_profile(State(st): State<AppState>) -> Result<Json<Value>, AppError> {
    let p = st.db.profile().await?;
    let s = st.hw.snapshot().await;
    let sys = &st.sys;   // the startup-captured machine facts; see the note above
    // Python's `if HAS_GPU:` block (main.py:178-186) is all-or-nothing: the dict literal is
    // fully evaluated before `prof.update`, so any failed NVML query emits `gpu_error` alone,
    // never a partial `gpu`/`gpu_vram_gb`/`gpu_driver` trio. `available()` is that flag, and
    // the "None detected" string is Python's own (main.py:182).
    let (gpu, gpu_vram_gb, gpu_driver, gpu_error) = if !st.gpu.available() {
        (Some("None detected".to_string()), None, None, None)
    } else {
        match (st.gpu.name(), st.gpu.vram_gb(), st.gpu.driver()) {
            (Some(name), Some(vram), Some(driver)) => (Some(name), Some(vram), Some(driver), None),
            // The trait reports a missing field, not the NVML error text, so the wording is
            // generic here (Python interpolates `str(e)`); spec deviation, values still parity.
            _ => (None, None, None,
                  Some(st.gpu.error().unwrap_or_else(|| "NVML query failed".into()))),
        }
    };
    let out = ProfileOut {
        name: p.name, notes: p.notes, cpu: sys.cpu.clone(),
        cpu_cores_physical: sys.cores_physical,
        cpu_cores_logical: sys.cores_logical,
        ram_total_gb: s.ram_total_gb, os: sys.os.clone(),
        gpu, gpu_vram_gb, gpu_driver, gpu_error,
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

/// Python's `stress_test` (main.py:197). The body goes through the shared 422 pass first
/// (`concurrency` 1..=32 and `requests` 1..=500 are bounds like `max_tokens`); then `requests`
/// tasks run behind a semaphore of `concurrency` permits, each timing one non-streaming
/// completion. A failed call never becomes an HTTP error — it lands in `errors`/`error_samples`
/// and the endpoint still answers 200 with the aggregate, because Python has no error path here
/// at all. The history row is written even when every call failed, also like Python, which
/// appends it after the gather unconditionally; a lost write is the one case Python cannot have
/// and this port reports through `AppError` rather than swallowing (ruling R11).
pub async fn stress_test(State(st): State<AppState>, body: Bytes) -> Response {
    let req = match ValidationErrors::parse_json_body::<StressReq>(&body, StressReq::FIELDS) {
        Ok(req) => req,
        Err(v) => return v.into_response(),
    };
    let base = resolve_base(&st.cfg, req.base_url.as_deref());
    let sem = Arc::new(Semaphore::new(req.concurrency as usize));
    let t0 = Instant::now();
    let mut handles = Vec::with_capacity(req.requests as usize);
    for _ in 0..req.requests {
        let (sem, http, base, req) = (sem.clone(), st.http.clone(), base.clone(), req.clone());
        handles.push(tokio::spawn(async move {
            // The permit is held until this block ends, so at most `concurrency` tasks are in
            // flight at once — Python's `async with sem`.
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => return Err("semaphore closed".to_string()),
            };
            let t = Instant::now();
            let payload = json!({ "model": req.model,
                "messages": [{"role": "user", "content": req.prompt}],
                "max_tokens": req.max_tokens });
            let r = match http.post(format!("{base}/v1/chat/completions"))
                .timeout(Duration::from_secs(300)).json(&payload).send().await
            {
                Ok(r) => r,
                Err(e) => return Err(e.to_string()),
            };
            // `Response::bytes` consumes the response, so read the status first. The body is
            // read before the elapsed time is taken and parsed after: httpx's `post()` returns
            // only once the body is read, and Python timestamps before `r.json()`.
            let code = r.status().as_u16();
            let bytes = match r.bytes().await {
                Ok(b) => b,
                Err(e) => return Err(e.to_string()),
            };
            let elapsed = t.elapsed().as_secs_f64();
            if code >= 400 {
                // Python's exact wording: the bare number, no reason phrase and no body.
                return Err(format!("HTTP {code}"));
            }
            match serde_json::from_slice::<Value>(&bytes) {
                Err(e) => Err(e.to_string()),
                Ok(body) => {
                    let tok = body.get("usage").and_then(|u| u.get("completion_tokens"))
                        .and_then(|c| c.as_i64()).unwrap_or(0);
                    Ok((elapsed, tok))
                }
            }
        }));
    }
    let (mut latencies, mut tokens_list, mut errors) =
        (Vec::new(), Vec::new(), Vec::<String>::new());
    for h in handles {
        match h.await {
            Ok(Ok((el, tk))) => { latencies.push(el); tokens_list.push(tk); }
            Ok(Err(msg)) => errors.push(msg),
            Err(e) => errors.push(format!("task join: {e}")),
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let agg = from_samples(&latencies, &tokens_list, &errors, latencies.len(), wall);

    let out = StressOut {
        model: req.model.clone(), base_url: base.clone(),
        concurrency: req.concurrency, requests: req.requests,
        success: agg.success, errors: agg.errors, error_samples: agg.error_samples,
        wall_s: agg.wall_s, avg_latency_s: agg.avg_latency_s, p95_latency_s: agg.p95_latency_s,
        total_tokens: agg.total_tokens, agg_tok_per_s: agg.agg_tok_per_s, req_per_s: agg.req_per_s,
    };
    // main.py:259-267's synthetic history entry: the model suffixed with the concurrency, no
    // TTFT, `total_s` = wall, `tok_per_s` = the aggregate throughput.
    let row = RunRow {
        kind: "stress".into(),
        timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        model: format!("{} (stress x{})", req.model, req.concurrency),
        max_tokens: req.max_tokens, ttft_ms: None, total_s: out.wall_s,
        tokens: out.total_tokens, tok_per_s: out.agg_tok_per_s,
        concurrency: Some(req.concurrency), base_url: Some(base),
    };
    if let Err(e) = st.db.insert_run(&row).await { return AppError::from(e).into_response(); }
    Json(out).into_response()
}

/// `GET /api/prompts`: the dashboard's question pool — the built-in 18 plus whatever a previous
/// fetch cached in SQLite, built-in entries first so the picker's order is stable across
/// restarts. A store failure stays an error envelope (`?` → `AppError`, the same 200-envelope
/// rule profile and history follow): serving "just the built-ins" would silently hide a broken
/// database, and the UI would report a healthy pool that silently shrinks.
pub async fn get_prompts(State(st): State<AppState>) -> Result<Json<Value>, AppError> {
    let fetched = st.db.list_prompts().await?;
    let pool = crate::prompts::merge(crate::prompts::built_in(), fetched);
    Ok(Json(json!({
        "defaults": crate::prompts::defaults(),
        "pool": pool.iter()
            .map(|q| json!({ "category": q.category, "text": q.text }))
            .collect::<Vec<_>>(),
    })))
}

/// `POST /api/prompts/fetch`: sample questions from every configured benchmark source and
/// REPLACE the cached set (re-fetching re-draws the sample rather than piling up rows). All
/// sources failing is a runtime failure — the 200 error envelope, pool untouched — while a
/// partial failure succeeds and names the dead sources in `failed`, so the UI can show what
/// was skipped. A fetch that lands nothing also never wipes the existing cache: the
/// `replace_prompts` write only happens on the success path.
pub async fn fetch_prompts(State(st): State<AppState>) -> Result<Json<Value>, AppError> {
    let (mut got, failed) = crate::prompts::fetch_all(&st.http, &st.cfg.prompts_sources).await;
    if got.is_empty() {
        let detail = failed.iter().map(|(c, e)| format!("{c}: {e}")).collect::<Vec<_>>()
            .join("; ");
        return Err(AppError(format!("question fetch failed ({detail})")));
    }
    got.truncate(crate::prompts::MAX_FETCHED);
    let added = got.len();
    let failed: Vec<Value> = failed.iter()
        .map(|(c, e)| json!({ "category": c, "error": e })).collect();
    st.db.replace_prompts(&got).await?;
    Ok(Json(json!({ "added": added, "failed": failed })))
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

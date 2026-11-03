use std::sync::Arc;
use std::time::{Duration, Instant};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
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

/// Truthiness on the optional upstream API key: absent, null, or whitespace-only send no
/// header — an empty `Bearer ` would be a guaranteed 401 from any key-checking gateway, and
/// the keyless providers (LM Studio, Ollama) must be untouched. Present and non-blank, the
/// value rides as `Authorization: Bearer <key>` on every completion call. The key is forwarded
/// only: never persisted with the history row, never logged.
fn bearer(api_key: &Option<String>) -> Option<String> {
    api_key.as_deref().map(str::trim).filter(|k| !k.is_empty())
        .map(|k| format!("Bearer {k}"))
}

/// `GET /api/version`: the version this binary was compiled from, read from the manifest at
/// build time. The About dialog renders it, so the page carries no second copy that could
/// drift from `Cargo.toml`.
pub async fn version() -> Json<Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

/// `base_url` is optional on every request: absent, empty, or whitespace-only all fall back to
/// the configured LM Studio URL, so `#[serde(default)]` on an `Option` is the shape.
#[derive(Deserialize)]
pub struct ModelQuery {
    #[serde(default)]
    pub base_url: Option<String>,
}

/// `list_models`: one GET `{base}/v1/models` with a 10 s timeout, and *any* failure answers
/// HTTP 200 with `{"error": ...}` — never a `Result`, because the frontend's `Array.isArray(m)`
/// check is what separates a model list from a failure. A success answers a bare array of id
/// strings, dropping entries whose `id` is missing, empty, or not a string.
///
/// The client is `AppState.http`, built once in `main.rs`, not a new client per request; the
/// per-request `.timeout(10s)` overrides the shared builder's ceiling for this call.
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

/// The `hw::Sampler` owns the CPU, RAM and GPU counters, so the handler just publishes the
/// cached snapshot. `Snapshot::to_json` fixes the key order.
///
/// `gpu_status` explains absent counters to the dashboard's driver hint. The probe closure is
/// lazy and cached: an available GPU answers `"ok"` without scanning PCI, and on Windows the
/// PowerShell query runs once per process, on the first poll that finds no NVML.
pub async fn hardware(State(st): State<AppState>) -> Json<Value> {
    let snap = st.hw.snapshot().await;
    let status = crate::hw::gpu_status(snap.gpu.is_some(), crate::hw::nvidia_present);
    Json(snap.to_json(status))
}

/// `GET /api/history`: the most recent sixty rows, oldest-first. A failed read is a runtime
/// failure, so it answers the mandated 200 `{"error": …}` through `AppError` instead of
/// `unwrap_or_default()` — an empty list is a legitimate answer ("no runs yet"), and it must
/// not be indistinguishable from "the database is unavailable". The dashboard's history pager
/// adds optional `?limit=`/`?offset=` windows over the whole dataset and an `X-Total-Runs`
/// header carrying the unfiltered row count, so it can render Prev/Next without a second
/// endpoint. The body shape never changes.
#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

pub async fn get_history(State(st): State<AppState>, Query(q): Query<HistoryQuery>)
    -> Result<Response, AppError>
{
    let (rows, total) = st.db.runs_page(
        q.limit.unwrap_or(60).clamp(1, 500), q.offset.unwrap_or(0).max(0)).await?;
    let mut resp = Json(Value::Array(rows.iter().map(|r| r.to_history_json()).collect()))
        .into_response();
    resp.headers_mut().insert(axum::http::HeaderName::from_static("x-total-runs"),
                              axum::http::HeaderValue::from(total));
    Ok(resp)
}

/// `DELETE /api/history/{id}`: the dashboard's per-row ✕. Removes exactly the named run and
/// answers `{"deleted": n, "total": newTotal}` — `total` is the fresh row count so the UI can
/// clamp its pager without a second request. An unknown id is `{"deleted": 0, "total": …}`
/// (still 200 — nothing failed), and a store failure rides the mandated `{"error": …}`
/// envelope through `AppError`. A non-numeric `{id}` never reaches the handler: axum's
/// `Path<i64>` answers it 400 before any state is touched.
pub async fn delete_run(State(st): State<AppState>, Path(id): Path<i64>)
    -> Result<Json<Value>, AppError>
{
    let (deleted, total) = st.db.delete_run(id).await?;
    Ok(Json(json!({ "deleted": deleted, "total": total })))
}

/// `GET /api/profile`: the mutable part (`name`, `notes`) comes from the SQLite `profile` row;
/// the immutable part from `ProfileStatic`, captured once at startup — rebuilding system info
/// per request is the per-call sampling cost this design avoids. `ram_total_gb` rides the
/// `Sampler`'s cached snapshot instead, and the `gpu*` fields come from the handle the sampler
/// already samples. `ProfileOut::to_json` fixes the wire key order.
/// A failed read is a runtime failure, so it answers the mandated 200 `{"error": …}` through
/// `AppError` rather than the factory profile: a `.unwrap_or(default)` would silently turn a
/// busy database into a "My workstation" the next Save would write back over the real row.
pub async fn get_profile(State(st): State<AppState>) -> Result<Json<Value>, AppError> {
    let p = st.db.profile().await?;
    let s = st.hw.snapshot().await;
    let sys = &st.sys;   // the startup-captured machine facts; see the note above
    // The GPU block is all-or-nothing: the profile fields are assembled in one shot, so any
    // failed NVML query emits `gpu_error` alone, never a partial `gpu`/`gpu_vram_gb`/
    // `gpu_driver` trio. `available()` is that flag, and "None detected" is the fallback name.
    let gpu_now = st.gpu.current();
    let (gpu, gpu_vram_gb, gpu_driver, gpu_error) = if !gpu_now.available() {
        (Some("None detected".to_string()), None, None, None)
    } else {
        match (gpu_now.name(), gpu_now.vram_gb(), gpu_now.driver()) {
            (Some(name), Some(vram), Some(driver)) => (Some(name), Some(vram), Some(driver), None),
            // The trait reports a missing field, not the NVML error text, so the wording is
            // generic here; the values still surface what the driver could report.
            _ => (None, None, None,
                  Some(gpu_now.error().unwrap_or_else(|| "NVML query failed".into()))),
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

/// `set_profile`: absent or `null` leaves the stored value unchanged, present (including `""`)
/// overwrites; the reply is the bare `{"name", "notes"}` object.
///
/// The body goes through the one shared mapping, `ValidationErrors::parse_json_body` — a
/// malformed body is the 422 `{"detail": [ ... ]}` envelope (never axum's plain-text 400), and
/// a wrong-typed `name`/`notes` is the `string_type` detail entry.
///
/// The `Err` arm is `AppError`, not a swallowed write: a failed `UPDATE profile` must answer the
/// mandated 200 `{"error": …}`, and must never answer the stale row, because the dashboard
/// prints "Profile saved" whatever the body says.
pub async fn set_profile(State(st): State<AppState>, body: Bytes) -> Result<Response, AppError> {
    let req = match ValidationErrors::parse_json_body::<ProfileReq>(&body, ProfileReq::FIELDS) {
        Ok(req) => req,
        Err(v) => return Ok(v.into_response()),
    };
    let p = st.db.set_profile(req.name.as_deref(), req.notes.as_deref()).await?;
    Ok(Json(json!({ "name": p.name, "notes": p.notes })).into_response())
}

/// `run_test`: validation (422) happens before the probe; a runtime failure is the mandated
/// 200 `{"error": …}` through `AppError`; a success is persisted through `Db::insert_run` so
/// `/api/history` sees it — the DB write failing is itself a runtime failure, answered
/// through `AppError` rather than swallowed, because a lost write must not look like a success.
pub async fn run_test(State(st): State<AppState>, body: Bytes) -> Response {
    // Type AND `max_tokens` bounds are checked by the one shared pass, so a 422 carries every
    // offending field at once (see `TestReq::FIELDS`).
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

/// `stress_test`: the body goes through the shared 422 pass first (`concurrency` 1..=32 and
/// `requests` 1..=500 are bounds like `max_tokens`); then `requests` tasks run behind a
/// semaphore of `concurrency` permits, each timing one non-streaming completion. A failed call
/// never becomes an HTTP error — it lands in `errors`/`error_samples` and the endpoint still
/// answers 200 with the aggregate. The history row is written even when every call failed; a
/// lost write is reported through `AppError` rather than swallowed.
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
            // flight at once.
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => return Err("semaphore closed".to_string()),
            };
            let t = Instant::now();
            let payload = json!({ "model": req.model,
                "messages": [{"role": "user", "content": req.prompt}],
                "max_tokens": req.max_tokens });
            let mut rb = http.post(format!("{base}/v1/chat/completions"));
            if let Some(a) = bearer(&req.api_key) {
                rb = rb.header(reqwest::header::AUTHORIZATION, a);
            }
            let r = match rb.timeout(Duration::from_secs(300)).json(&payload).send().await
            {
                Ok(r) => r,
                Err(e) => return Err(e.to_string()),
            };
            // `Response::bytes` consumes the response, so read the status first. The body is
            // read before the elapsed time is taken and parsed after, so the timing covers
            // body transfer rather than stopping at the response headers.
            let code = r.status().as_u16();
            let bytes = match r.bytes().await {
                Ok(b) => b,
                Err(e) => return Err(e.to_string()),
            };
            let elapsed = t.elapsed().as_secs_f64();
            if code >= 400 {
                // The bare number, no reason phrase and no body.
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
    // The synthetic history entry for a stress run: the model suffixed with the concurrency,
    // no TTFT, `total_s` = wall, `tok_per_s` = the aggregate throughput.
    let row = RunRow {
        id: 0,
        kind: "stress".into(),
        timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
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

/// The whole probe against `base`, no Axum types: two upstream calls — a streaming one-token
/// TTFT probe (60 s) and a full non-streaming completion (300 s) whose
/// `usage.completion_tokens / elapsed` is the token rate. Returns the wire value and the row
/// to persist. The client is the shared `AppState.http`; the per-request `.timeout()` calls
/// override the builder's 300 s ceiling for each call.
async fn probe(req: &TestReq, cfg: &Config, http: &reqwest::Client)
    -> Result<(Value, RunRow), AppError>
{
    let base = resolve_base(cfg, req.base_url.as_deref());
    let auth = bearer(&req.api_key);

    // 1) TTFT: stream ONE token — `"max_tokens": 1`, not `req.max_tokens`; streaming the full
    //    budget would measure total time, not first-token time.
    let payload = json!({
        "model": req.model, "messages": [{"role": "user", "content": req.prompt}],
        "max_tokens": 1, "stream": true });
    let t0 = Instant::now();
    let mut rb = http.post(format!("{base}/v1/chat/completions"));
    if let Some(a) = &auth { rb = rb.header(reqwest::header::AUTHORIZATION, a); }
    let r = rb.timeout(Duration::from_secs(60)).json(&payload).send().await
        .map_err(|e| AppError(format!("TTFT probe failed: {e}")))?;
    if r.status().as_u16() >= 400 {
        // Format `HTTP {code}` — the bare number, not hyper's "400 Bad Request".
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
    //    taken, so the timing includes body transfer rather than stopping at the headers.
    let t1 = Instant::now();
    let payload2 = json!({
        "model": req.model, "messages": [{"role": "user", "content": req.prompt}],
        "max_tokens": req.max_tokens });
    let mut rb = http.post(format!("{base}/v1/chat/completions"));
    if let Some(a) = &auth { rb = rb.header(reqwest::header::AUTHORIZATION, a); }
    let r2 = rb.timeout(Duration::from_secs(300)).json(&payload2).send().await
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

    // The *unrounded* TTFT decides: a zero value emits `ttft_ms: null` (the key is never
    // dropped; a probe that raised never reaches this struct at all).
    let out = TestOut {
        timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        model: req.model.clone(), max_tokens: req.max_tokens,
        ttft_ms: ttft_ms.filter(|v| *v != 0.0).map(|v| round_half_even(v, 1)),
        total_s: round_half_even(elapsed, 2), tokens, tok_per_s: tps };
    let row = RunRow {
        id: 0,
        kind: "single".into(), timestamp: out.timestamp.clone(), model: out.model.clone(),
        max_tokens: out.max_tokens, ttft_ms: out.ttft_ms, total_s: out.total_s,
        tokens: out.tokens, tok_per_s: out.tok_per_s, concurrency: None, base_url: None };
    let value = serde_json::to_value(&out)
        .map_err(|e| AppError(format!("failed to serialize the test result: {e}")))?;
    Ok((value, row))
}

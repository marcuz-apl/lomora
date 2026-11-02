use axum::Json;
use axum::extract::{Query, State};
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::config::resolve_base;
use crate::error::{AppError, ValidationErrors};
use crate::models::{ProfileOut, ProfileReq, TestReq};

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
/// The extractor is load-bearing, not decoration, for the same reason as
/// `run_test_placeholder`: the brief's plain `Json(req)` would answer a malformed body with
/// axum's plain-text 400, while the binding constraint (and FastAPI) demand 422 `{"detail":
/// [ ... ]}`. Both rejection sites share `ValidationErrors::from_json_rejection`.
///
/// The `Err` arm is `AppError`, not a swallowed write: a failed `UPDATE profile` must answer the
/// mandated 200 `{"error": …}`, and must never answer the stale row, because
/// `static/index.html:329` prints "Profile saved" whatever the body says. The data-error vs
/// syntax-error split stays Task 5's job, as in `run_test_placeholder`.
pub async fn set_profile(
    State(st): State<AppState>,
    body: Result<Json<ProfileReq>, JsonRejection>,
) -> Result<Response, AppError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(rejection) => return Ok(ValidationErrors::from_json_rejection(&rejection)),
    };
    let p = st.db.set_profile(req.name.as_deref(), req.notes.as_deref()).await?;
    Ok(Json(json!({ "name": p.name, "notes": p.notes })).into_response())
}

/// Placeholder until Task 5 implements the benchmark. The extractor is load-bearing, not
/// decoration: `Result<Json<_>, _>` lets axum parse the body and hand the rejection here,
/// so a malformed body answers 422 with a `detail` array exactly like FastAPI does, while
/// `Json<TestReq>` alone would answer a plain-text 400.
pub async fn run_test_placeholder(body: Result<Json<TestReq>, JsonRejection>) -> Response {
    match body {
        Ok(Json(_req)) => Json(json!({"error": "not implemented"})).into_response(),
        // axum's JsonRejection exposes no parse offset (see ValidationErrors::json_invalid), so
        // the position is reported as 0 for now; Task 5 owns the real offset and the data-error
        // vs syntax-error split.
        Err(rejection) => ValidationErrors::from_json_rejection(&rejection),
    }
}

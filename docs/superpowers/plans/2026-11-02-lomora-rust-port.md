# Lomora Rust Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rewrite Lomora's FastAPI backend as a single Rust binary that is wire-compatible with the eight existing endpoints, then delete the Python implementation.

**Architecture:** Axum 0.8 on Tokio, with an `AppState` holding one shared `reqwest::Client`, a background hardware sampler publishing snapshots, and a single `rusqlite` connection behind a `tokio::sync::Mutex`. All measurement logic lives in `bench.rs` as pure functions with no I/O so it is unit-testable against hand-written byte chunks. Handlers stay thin and return HTTP 200 with `{"error": ...}` exactly as Python does, because the unchanged frontend depends on that shape.

**Tech Stack:** Rust 1.97.1 (edition 2024), axum 0.8.9, tokio 1.54.1, reqwest 0.13.5, tower-http 0.7.1, rusqlite 0.41.1 (bundled), sysinfo 0.40.1, nvml-wrapper 0.13.0, rust-embed 8.13.0, serde 1.0.229.

**Spec:** `docs/superpowers/specs/2026-11-02-lomora-rust-port-design.md` — read it alongside this plan. Section references below are to that file.

---

## Global Constraints

- The frontend `static/index.html` is **not** modified in Tasks 1-8. Any change there is a spec violation.
- Endpoint paths, methods, and JSON key names are unchanged from `main.py`. Keys not present in a Python response must not appear in the Rust response (no `null` for omitted GPU keys).
- JSON key **order** matches Python's dict insertion order. `serde_json` emits struct fields in declaration order, so declaration order in `models.rs` is load-bearing for byte-level diffs.
- Every rounding operation uses round-half-to-even, never `f64::round`.
- `/api/history` returns oldest-first, at most 60 rows.
- Runtime failures return HTTP **200** with `{"error": "<message>"}`. Request validation failures and malformed bodies return HTTP **422** with `{"detail": [...]}`.
- `/api/stress` has **no** error path: an unreachable upstream yields `success: 0` and `null` latency fields with a 200.
- Default `max_tokens` is 100 (`/api/test`) and 64 (`/api/stress`); ranges are 1..=2048, concurrency 1..=32, requests 1..=500.
- Upstream timeouts: 60 s TTFT probe, 300 s inference, 300 s per stress request, 10 s models list.
- GPU reads index 0 only, matching Python, even though this machine has two Quadro P5000 cards.
- Build must stay warning-free: `cargo clippy --all-targets -- -D warnings`.
- `LMSTUDIO_URL` default `http://localhost:1234`, `OLLAMA_URL` default `http://localhost:11434`, `PORT` default 8501 bound on `0.0.0.0`, `LOMORA_DB` default `lomora.db`.
- During Tasks 1-8 the Rust server runs on **port 8502** so Python keeps `:8501` for diffing. Live upstream calls pass an explicit Ollama `base_url`; LM Studio is not running on this machine.
- `gemma4:12b` loads exactly once, in Task 8. Tasks 1-7 use the in-process mock.

## File Structure

`rust/src/lib.rs` and `rust/src/main.rs` split so integration tests in `rust/tests/` can import the router; a single `main.rs` would make that impossible.

```
rust/
  Cargo.toml                 lib + bin targets, dependency features
  src/
    lib.rs                   module declarations only
    main.rs                  env -> Config -> AppState -> spawn sampler -> serve
    app.rs                   build_router(AppState) -> Router; CORS; static mount
    config.rs                Config::from_env, normalize_base
    error.rs                 AppError (200 + error), ValidationErrors (422 + detail)
    models.rs                TestReq, StressReq, ProfileReq, response structs
    routes.rs                the eight handlers
    bench.rs                 round_half_even, SseScanner, p95, StressAggregate
    hw.rs                    Snapshot, Gpu trait, NvmlGpu, NoGpu, Sampler
    store.rs                 Db, RunRow, schema, insert/recent/profile
    assets.rs                RustEmbed struct + embedded file handler
  tests/
    support/mod.rs           spawn_mock(), spawn_app(), request helpers
    scaffold.rs              Task 1
    hardware.rs              Task 2
    models_endpoint.rs       Task 3
    profile.rs               Task 4
    test_endpoint.rs         Task 5
    stress_endpoint.rs       Task 6
    history_persistence.rs   Task 7
    parity.rs                Task 8 (#[ignore], needs both servers live)
```

Responsibilities: `bench.rs` never touches I/O or clocks it does not receive as arguments. `hw.rs` is the only module that reads system counters. `store.rs` is the only module that knows SQL. `routes.rs` composes the others and holds no logic of its own.

---

## Task 1: Crate scaffold, error shape, embedded static serving

**Files:**
- Create: `rust/Cargo.toml`, `rust/src/lib.rs`, `rust/src/main.rs`, `rust/src/app.rs`, `rust/src/config.rs`, `rust/src/error.rs`, `rust/src/assets.rs`, `rust/src/models.rs`, `rust/src/routes.rs`
- Create: `rust/tests/support/mod.rs`, `rust/tests/scaffold.rs`
- Modify: `.gitignore` (append `rust/target/`, `lomora.db`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `Config { lmstudio_url: String, ollama_url: String, port: u16, db_path: PathBuf, static_dir: Option<PathBuf> }`, `Config::from_env() -> Config`, `normalize_base(&str) -> String`
  - `AppError(pub String)` implementing `IntoResponse` (status 200, body `{"error": string}`)
  - `ValidationErrors::from_field(field: &str, msg: &str, kind: &str) -> ValidationErrors` implementing `IntoResponse` (status 422, body `{"detail":[{"type","loc":["body",field],"msg","input":null}]}`)
  - `build_router(state: AppState) -> axum::Router`
  - `AppState` — fields `http`, `hw`, `db`, `cfg` are created in Tasks 2-4; in this task it holds only `cfg: Arc<Config>`
  - `GET /api/history` handler returning `Json<Vec<serde_json::Value>>` of `[]`

- [ ] **Step 1: Create the crate manifest**

`rust/Cargo.toml`:

```toml
[package]
name = "lomora"
version = "0.1.0"
edition = "2024"

[lib]
name = "lomora"
path = "src/lib.rs"

[[bin]]
name = "lomora"
path = "src/main.rs"

[dependencies]
axum = "0.8.9"
tokio = { version = "1.54.1", features = ["full"] }
reqwest = { version = "0.13.5", features = ["json", "stream"] }
tower-http = { version = "0.7.1", features = ["cors"] }
rusqlite = { version = "0.41.1", features = ["bundled"] }
sysinfo = "0.40.1"
nvml-wrapper = "0.13.0"
rust-embed = "8.13.0"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
futures-util = "0.3.34"
mime_guess = "2.0.5"

[dev-dependencies]
tokio = { version = "1.54.1", features = ["full"] }
```

- [ ] **Step 2: Write the failing tests for the error contract and static serving**

`rust/tests/scaffold.rs`:

```rust
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
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cd rust && cargo test --test scaffold`
Expected: compile failure — `could not find`, the crate does not exist yet.

- [ ] **Step 4: Write `config.rs`**

```rust
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub lmstudio_url: String,
    pub ollama_url: String,
    pub port: u16,
    pub db_path: PathBuf,
    pub static_dir: Option<PathBuf>,
}

/// Trailing-slash strip, mirroring `os.getenv(...).rstrip("/")` in main.py.
pub fn normalize_base(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            lmstudio_url: normalize_base(&env_or("LMSTUDIO_URL", "http://localhost:1234")),
            ollama_url:   normalize_base(&env_or("OLLAMA_URL",   "http://localhost:11434")),
            port: env_or("PORT", "8501").parse().unwrap_or(8501),
            db_path: PathBuf::from(env_or("LOMORA_DB", "lomora.db")),
            static_dir: std::env::var("LOMORA_STATIC").ok()
                .filter(|v| !v.trim().is_empty()).map(PathBuf::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_base;
    #[test]
    fn strips_one_or_many_trailing_slashes() {
        assert_eq!(normalize_base("http://x:1/"), "http://x:1");
        assert_eq!(normalize_base("http://x:1///"), "http://x:1");
        assert_eq!(normalize_base("http://x:1"), "http://x:1");
    }
}
```

- [ ] **Step 5: Write `error.rs`**

```rust
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Python returns HTTP 200 with {"error": ...}; index.html reads data.error after
/// r.json(). A 4xx here would break the dashboard silently.
pub struct AppError(pub String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(json!({ "error": self.0 }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> AppError { AppError(e.to_string()) }
}

/// pydantic answers 422 with a "detail" array before the handler runs. Mirrored, not
/// "fixed" into a friendly 200, because wire parity is the goal. See spec 5.1.
pub struct ValidationErrors(pub serde_json::Value);

impl ValidationErrors {
    pub fn from_field(field: &str, msg: &str, kind: &str) -> ValidationErrors {
        ValidationErrors(json!({ "detail": [
            { "type": kind, "loc": ["body", field], "msg": msg, "input": null }
        ]}))
    }
}

impl IntoResponse for ValidationErrors {
    fn into_response(self) -> Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(self.0)).into_response()
    }
}
```

Add `anyhow = "1"` to `[dependencies]` in `Cargo.toml` in this step.

- [ ] **Step 6: Write `assets.rs` for embedded static files**

```rust
use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../static"]
pub struct Asset;

pub async fn serve(path: Option<String>) -> Response<Body> {
    let rel = match path { Some(p) if !p.is_empty() => p.trim_start_matches('/'), _ => "index.html".into() };
    if let Some(f) = Asset::get(&rel) {
        let ctype = mime_guess::from_path(&rel).first_or_text_plain().to_string();
        return (StatusCode::OK, [("content-type", ctype)], String::from_utf8_lossy(f.data.as_ref()).into_owned())
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

pub async fn serve_dir(dir: std::path::PathBuf, path: Option<String>) -> Response<Body> {
    let rel = match path { Some(p) if !p.is_empty() => p.trim_start_matches('/').to_string(), _ => "index.html".into() };
    let file = dir.join(&rel);
    if file.is_file() {
        let bytes = tokio::fs::read(&file).await.unwrap_or_default();
        let ctype = mime_guess::from_path(&rel).first_or_text_plain().to_string();
        return (StatusCode::OK, [("content-type", ctype)], Body::from(bytes)).into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}
```

`rust-embed` resolves `folder` relative to the crate root, so `../static` is correct while the crate lives in `rust/`. Task 9 changes it to `static` when the crate moves to the repo root.

- [ ] **Step 7: Write `app.rs`, `models.rs`, `routes.rs`, `lib.rs`, `main.rs`**

`rust/src/models.rs` — only the request types are needed now; response structs arrive in later tasks.

```rust
use serde::{Deserialize, Serialize};

fn default_model() -> String { "local-model".into() }
fn default_prompt() -> String { "Count from 1 to 20, one number per line.".into() }
fn default_maxtok_test() -> i64 { 100 }

#[derive(Deserialize, Clone)]
pub struct TestReq {
    #[serde(default = "default_model")]   pub model: String,
    #[serde(default = "default_prompt")]  pub prompt: String,
    #[serde(default = "default_maxtok_test")] pub max_tokens: i64,
    #[serde(default)]                     pub base_url: Option<String>,
}

#[derive(Serialize)]
pub struct EmptyHistory;
```

`rust/src/routes.rs`:

```rust
use axum::Json;
use serde_json::{Value, json};

/// Placeholder until Task 7 wires this to SQLite. Python returns oldest-first.
pub async fn get_history() -> Json<Value> {
    Json(json!([]))
}
```

`rust/src/app.rs` — note axum 0.8 catch-all syntax is `{*rest}`, not `/*rest`, and that
`fallback` takes a plain handler (an `any(...)` `MethodRouter` will not compile there).

```rust
use std::sync::Arc;
use axum::body::Body;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use crate::config::Config;

#[derive(Clone)]
pub struct AppState { pub cfg: Arc<Config> }

impl AppState {
    /// The single construction path. Tasks 2, 3, 4 and 6 each add one field here — to the
    /// struct, to `new`, and to no other call site, because `main.rs`, `spawn_app`, and
    /// `fresh_app` all go through this function.
    pub fn new(cfg: Config) -> AppState { AppState { cfg: Arc::new(cfg) } }
}

async fn fallback(State(st): State<AppState>, uri: Uri) -> Response<Body> {
    let path = uri.path().to_string();
    match &st.cfg.static_dir {
        Some(dir) => crate::assets::serve_dir(dir.clone(), Some(path)).await,
        None => crate::assets::serve(Some(path)).await,
    }
}

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);
    Router::new()
        .route("/api/history", get(crate::routes::get_history))
        .route("/api/test", post(crate::routes::run_test_placeholder))
        .fallback(fallback)
        .layer(cors)
        .with_state(state)
}
```

`AppState` in this task holds **only** `cfg` — `hw` arrives in Task 2, `http` in Task 3,
`db` and `sys` in Task 4, `gpu` in Task 6. Do not add a field early to "save a round trip":
`hw.rs` does not exist yet, so a `Sampler` field here cannot compile. `main.rs` and both test
spawners call `AppState::new(cfg)`, so a field added in a later task is a one-place change.

`rust/src/lib.rs` declares every module that exists, including `routes` and (from Task 2)
`hw` and `bench`:

```rust
pub mod app;
pub mod assets;
pub mod config;
pub mod error;
pub mod models;
pub mod routes;
```

`rust/src/main.rs`:

```rust
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = lomora::config::Config::from_env();
    let port = cfg.port;
    let state = lomora::app::AppState::new(cfg);
    let router = lomora::app::build_router(state);
    let addr = format!("0.0.0.0:{port}");
    println!("Lomora listening on http://{addr}");
    axum::serve(TcpListener::bind(&addr).await?, router).await?;
    Ok(())
}
```

`AppState::new(cfg)` is introduced in Task 5 Step 7; until then construct the struct
literally with the fields that exist.

- [ ] **Step 8: Replace the placeholder `/api/test` route so the 422 test is meaningful**

The Step 7 stub ignores the body, so `malformed_json_body_is_422_not_200` cannot pass against it. Change it to a real extractor so axum performs the validation the test asserts:

```rust
.route("/api/test", post(crate::routes::run_test_placeholder))
```

and in `routes.rs`:

```rust
use crate::models::TestReq;

pub async fn run_test_placeholder(Json(_req): Json<TestReq>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"error": "not implemented"}))
}
```

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cd rust && cargo test --test scaffold`
Expected: 3 passed, 0 failed.

Run: `cd rust && cargo clippy --all-targets -- -D warnings`
Expected: no warnings. Fix any before committing.

- [ ] **Step 10: Verify in a browser and diff against Python**

Run: `cd rust && PORT=8502 cargo run`
Open `http://localhost:8502`. Expected: the dashboard renders with the same layout as `http://localhost:8501`; hardware cards show `--`/`N/A` and history shows the empty-state row, both correct for this stage.

Run: `curl -s localhost:8501/api/history` and `curl -s localhost:8502/api/history`
Expected: both `[]`.

- [ ] **Step 11: Commit**

```bash
git add rust .gitignore
git commit -m "Add Rust crate scaffold with Axum router, error parity, embedded UI"
```

---

## Task 2: Hardware sampler and `GET /api/hardware`

**Files:**
- Create: `rust/src/hw.rs`, `rust/tests/hardware.rs`
- Modify: `rust/src/lib.rs` (add `pub mod hw;`), `rust/src/app.rs` (`AppState` gains `hw`), `rust/src/routes.rs`, `rust/src/main.rs`

**Interfaces:**
- Consumes: `AppState` from Task 1.
- Produces:
  - `Snapshot { cpu_percent: f64, ram_percent: f64, ram_used_gb: f64, ram_total_gb: f64, gpu: Option<GpuSnapshot> }` (`Clone`, `Copy`-compatible via `Option<GpuSnapshot>`)
  - `GpuSnapshot { util: u32, mem_used_mb: u64, mem_total_mb: u64, temp_c: i32 }`
  - `trait Gpu: Send + Sync { fn sample(&self) -> Option<GpuSnapshot>; fn name(&self) -> Option<String>; fn vram_gb(&self) -> Option<f64>; fn driver(&self) -> Option<String>; fn error(&self) -> Option<String> }`
  - `struct NoGpu; struct NvmlGpu { ... }`
  - `Sampler(Arc<tokio::sync::RwLock<Snapshot>>)` with `Sampler::new(first: Snapshot)`, `Sampler::snapshot(&self) -> Snapshot`, `async fn spawn(gpu: Arc<dyn Gpu>, interval: Duration) -> Sampler`
  - `routes::hardware(State<AppState>) -> Json<Value>`

- [ ] **Step 1: Calibration probe — settle the sysinfo memory unit with evidence**

Docs are ambiguous about whether sysinfo 0.40 returns bytes or KB, and this decides the `ram_gb` math, so measure instead of trusting the doc.

```bash
cd rust && mkdir -p examples && cat > examples/calib.rs <<'EOF'
use sysinfo::System;
fn main() {
    let mut s = System::new_all();
    s.refresh_all();
    std::thread::sleep(std::time::Duration::from_millis(400));
    s.refresh_cpu_usage();
    println!("used={:?} total={:?} cpu={:?} phys={:?}",
        s.used_memory(), s.total_memory(), s.global_cpu_usage(), s.physical_core_count());
}
EOF
cargo run --example calib
```

Then read the truth from the running Python app: `curl -s localhost:8501/api/hardware`.
Record in the commit message which unit sysinfo reported. If `total_memory()` is ~1000x the Python `ram_total_gb * 1024^3`, the unit is bytes; convert with `/ 1024^3`. If it matches `ram_total_gb * 1000^3`, it is decimal KB/MB — do not guess, derive the divisor so the printed `ram_total_gb` equals the Python value within 0.01. The acceptance test below is the enforcement.

- [ ] **Step 2: Write the failing hardware tests**

`rust/tests/hardware.rs`:

```rust
mod support;
use support::spawn_app;
use serde_json::Value;

#[tokio::test]
async fn hardware_reports_the_four_base_keys_in_python_order() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/hardware")).await.unwrap().json().await.unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert!(keys.starts_with(&["cpu_percent", "ram_percent", "ram_used_gb", "ram_total_gb"]), "{keys:?}");
    assert!(v["ram_total_gb"].as_f64().unwrap() > 0.5, "ram_total_gb looks wrong: {}", v["ram_total_gb"]);
    assert!(v["ram_used_gb"].as_f64().unwrap() <= v["ram_total_gb"].as_f64().unwrap());
    assert!(v["cpu_percent"].as_f64().unwrap() <= 100.0);
}

#[tokio::test]
async fn gpu_keys_are_present_or_absent_never_null() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/hardware")).await.unwrap().json().await.unwrap();
    match v.get("gpu_util") {
        Some(u) => {
            assert!(u.as_u64().unwrap() <= 100);
            assert!(v["gpu_mem_tot"].as_u64().unwrap() >= 1024, "VRAM total in MB looks wrong");
            assert!(v["gpu_mem_mb"].as_u64().unwrap() <= v["gpu_mem_tot"].as_u64().unwrap());
        }
        None => {
            assert!(v.get("gpu_mem_mb").is_none() && v.get("gpu_temp_c").is_none());
        }
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cd rust && cargo test --test hardware`
Expected: FAIL — `GET /api/hardware` falls through to the static handler and returns `not found`.

- [ ] **Step 4: Write `hw.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Clone, Copy, Debug, Default)]
pub struct GpuSnapshot { pub util: u32, pub mem_used_mb: u64, pub mem_total_mb: u64, pub temp_c: i32 }

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cpu_percent: f64, pub ram_percent: f64,
    pub ram_used_gb: f64, pub ram_total_gb: f64,
    pub gpu: Option<GpuSnapshot>,
}

pub trait Gpu: Send + Sync {
    fn sample(&self) -> Option<GpuSnapshot>;
    fn name(&self) -> Option<String>;
    fn vram_gb(&self) -> Option<f64>;
    fn driver(&self) -> Option<String>;
    fn error(&self) -> Option<String> { None }
}

pub struct NoGpu;
impl Gpu for NoGpu {
    fn sample(&self) -> Option<GpuSnapshot> { None }
    fn name(&self) -> Option<String> { None }
    fn vram_gb(&self) -> Option<f64> { None }
    fn driver(&self) -> Option<String> { None }
}

pub struct NvmlGpu { nvml: nvml_wrapper::Nvml, index: u32 }

impl NvmlGpu {
    pub fn open(index: u32) -> Option<NvmlGpu> {
        match nvml_wrapper::Nvml::init() {
            Ok(nvml) if nvml.device_by_index(index).is_ok() => Some(NvmlGpu { nvml, index }),
            _ => None,
        }
    }
}

impl Gpu for NvmlGpu {
    fn sample(&self) -> Option<GpuSnapshot> {
        let dev = self.nvml.device_by_index(self.index).ok()?;
        let util = dev.utilization_rates().ok()?;
        let mem  = dev.memory_usage().ok()?;
        let temp = dev.temperature().ok()?;
        Some(GpuSnapshot {
            util: util.gpu,
            mem_used_mb: mem.used / 1_048_576,
            mem_total_mb: mem.total / 1_048_576,
            temp_c: temp as i32,
        })
    }
    fn name(&self) -> Option<String> { self.nvml.device_by_index(self.index).ok()?.name().ok() }
    fn vram_gb(&self) -> Option<f64> {
        let m = self.nvml.device_by_index(self.index).ok()?.memory_usage().ok()?;
        Some(crate::bench::round_half_even(m.total as f64 / 1024f64.powi(3), 2))
    }
    fn driver(&self) -> Option<String> { self.nvml.sys_driver_version().ok() }
}

#[derive(Clone)]
pub struct Sampler(Arc<RwLock<Snapshot>>);

impl Sampler {
    pub fn new(first: Snapshot) -> Sampler { Sampler(Arc::new(RwLock::new(first))) }
    pub async fn snapshot(&self) -> Snapshot { *self.0.read().await }

    /// Background loop. sysinfo needs two refreshes to compute a usage percentage, so
    /// the very first tick is discarded rather than published.
    pub async fn spawn(gpu: Arc<dyn Gpu>, interval: Duration) -> Sampler {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_all();
        let sampler = Sampler::new(Snapshot::default());
        let shared = sampler.0.clone();
        tokio::spawn(async move {
            let mut first = true;
            loop {
                tokio::time::sleep(interval).await;
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                let total = sys.total_memory() as f64;
                let used  = sys.used_memory() as f64;
                // Apply the divisor established in Step 1 so GB values match Python.
                let (gb_used, gb_total) = (used / 1_073_741_824.0, total / 1_073_741_824.0);
                let snap = Snapshot {
                    cpu_percent: crate::bench::round_half_even(sys.global_cpu_usage() as f64, 1),
                    ram_percent: if total > 0.0 { crate::bench::round_half_even(used / total * 100.0, 1) } else { 0.0 },
                    ram_used_gb: crate::bench::round_half_even(gb_used, 2),
                    ram_total_gb: crate::bench::round_half_even(gb_total, 2),
                    gpu: gpu.sample(),
                };
                if first { first = false; continue; }
                *shared.write().await = snap;
            }
        });
        sampler
    }
}
```

`nvml-wrapper` 0.13 renamed accessors and `temperature()` may return a `Temperature` wrapper rather than an integer. If `cargo build` rejects `mem.total` or `temp as i32`, read the compiler's suggestion and the `docs.rs/nvml-wrapper/0.13.0` entries for `MemoryUsage` and `temperature`, then use the field or conversion the compiler names — expected ground truth on this machine is 16384 MiB total and driver `582.67`, so a wrong accessor that compiles still fails the test.

- [ ] **Step 5: Add the rounding helper to `bench.rs`, ahead of Task 5, because `hw.rs` needs it**

Create `rust/src/bench.rs`:

```rust
/// Python's round() is round-half-to-even; f64::round is half-away-from-zero and the two
/// disagree on exact ties. Rust's float formatting is correctly rounded, so format-then-
/// parse reproduces CPython. Spec 5.1.
pub fn round_half_even(x: f64, digits: u32) -> f64 {
    match format!("{:.*}", digits as usize, x).parse::<f64>() {
        Ok(v) => v,
        Err(_) => x,
    }
}
```

```rust
#[cfg(test)]
mod tests {
    use super::round_half_even;
    #[test]
    fn rounds_half_to_even_not_away() {
        assert_eq!(round_half_even(0.125, 2), 0.12);
        assert_eq!(round_half_even(0.135, 2), 0.14);
        assert_eq!(round_half_even(1.5, 0), 1.0);
        assert_eq!(round_half_even(2.5, 0), 2.0);
    }
    #[test]
    fn leaves_clean_values_alone() {
        assert_eq!(round_half_even(23.5, 1), 23.5);
        assert_eq!(round_half_even(1200.0, 1), 1200.0);
    }
}
```

Add `pub mod bench;` to `lib.rs`.

- [ ] **Step 6: Wire the route and state**

In `app.rs`, extend `AppState` with `pub hw: Sampler`, and add `.route("/api/hardware", get(crate::routes::hardware))`. In `routes.rs`:

```rust
use axum::extract::State;
use crate::app::AppState;
use serde_json::json;

pub async fn hardware(State(st): State<AppState>) -> Json<Value> {
    let s = st.hw.snapshot().await;
    let mut v = json!({
        "cpu_percent": s.cpu_percent,
        "ram_percent": s.ram_percent,
        "ram_used_gb":   s.ram_used_gb,
        "ram_total_gb":  s.ram_total_gb,
    });
    if let Some(g) = s.gpu {
        v["gpu_util"] = json!(g.util);
        v["gpu_mem_mb"] = json!(g.mem_used_mb);
        v["gpu_mem_tot"] = json!(g.mem_total_mb);
        v["gpu_temp_c"] = json!(g.temp_c);
    }
    Json(v)
}
```

Note the four GPU keys are inserted after the base four, matching Python's `d.update(...)`.

- [ ] **Step 7: Update `main.rs` and `tests/support/mod.rs` for the new field**

`main.rs` builds the GPU and sampler before the state:

```rust
let gpu: std::sync::Arc<dyn lomora::hw::Gpu> = match lomora::hw::NvmlGpu::open(0) {
    Some(g) => std::sync::Arc::new(g),
    None => std::sync::Arc::new(lomora::hw::NoGpu),
};
let hw = lomora::hw::Sampler::spawn(gpu, std::time::Duration::from_millis(500)).await;
let state = lomora::app::AppState { cfg: std::sync::Arc::new(cfg), hw };
```

`rust/tests/support/mod.rs` gains the app spawner every later task uses. `spawn_app` returns the base URL plus the server `JoinHandle`, binds port 0 and reads back the assigned port:

```rust
#![allow(dead_code)]
use std::net::SocketAddr;
use tokio::net::TcpListener;

pub async fn spawn_app() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = lomora::config::Config::from_env();
    let gpu = std::sync::Arc::new(lomora::hw::NoGpu);
    let hw = lomora::hw::Sampler::spawn(gpu, std::time::Duration::from_millis(60)).await;
    let state = lomora::app::AppState { cfg: std::sync::Arc::new(cfg), hw };
    let handle = tokio::spawn(async move {
        axum::serve(listener, lomora::app::build_router(state)).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}
```

- [ ] **Step 8: Run to verify they pass**

Run: `cd rust && cargo test --test hardware && cargo test --lib`
Expected: hardware tests pass; `round_half_even` tests pass.

- [ ] **Step 9: Live diff against Python**

Run: `cd rust && PORT=8502 cargo run`, then compare `curl -s localhost:8501/api/hardware` with `curl -s localhost:8502/api/hardware`.
Expected: same key names and order; `ram_total_gb` equal exactly; `ram_percent` within about 2 points; `gpu_mem_tot` exactly 16384 on the Rust side and 16384 on the Python side; `gpu_util` differs freely because it is instantaneous. If `ram_total_gb` differs at all, the Step 1 divisor is wrong — fix it, do not widen the test.

- [ ] **Step 10: Commit**

```bash
git add rust
git commit -m "Add background hardware sampler and /api/hardware parity route"
```

---

## Task 3: Shared HTTP client and `GET /api/models`

**Files:**
- Create: `rust/src/mock.rs`, `rust/src/bin/mock_upstream.rs`, `rust/tests/models_endpoint.rs`
- Modify: `rust/src/lib.rs` (`pub mod mock;`), `rust/src/app.rs` (`AppState` gains `http`), `rust/src/routes.rs`, `rust/src/main.rs`, `rust/tests/support/mod.rs` (re-export only)

**Interfaces:**
- Consumes: `Config::lmstudio_url`, `normalize_base`, `AppError`.
- Produces:
  - `AppState.http: reqwest::Client` — one shared client, pool reused, no per-request construction
  - `fn resolve_base(cfg: &Config, explicit: Option<&str>) -> String` — explicit wins, else `lmstudio_url`
  - `routes::list_models(State<AppState>, Query<ModelQuery>) -> Json<Value>` — always 200;
    the error case is a `{"error": ...}` body, not a `Result`
  - mock upstream from `tests/support` used by every later task

- [ ] **Step 1: Write the mock upstream as `rust/src/mock.rs`, not inside `tests/`**

It answers the three shapes the app consumes: an OpenAI model list, a non-streaming completion with `usage`, and a streaming completion whose frames are deliberately split mid-line so the SSE scanner is exercised across chunk boundaries.

It lives in the library because Task 8 needs the *same* mock running as a separate process that the Python reference server can call — and neither an `examples/` nor a `src/bin/` target can import code from `tests/`, which is not part of the crate. `rust/tests/support/mod.rs` re-exports it instead of defining it:

```rust
// rust/tests/support/mod.rs — top of file, above spawn_app
pub use lomora::mock::{peak_inflight, spawn_limited_mock, spawn_mock, spawn_mock_sparse};
```

Add `pub mod mock;` to `rust/src/lib.rs`. The functions below are all `pub` in `mock.rs`,
and `mock.rs` also exposes `pub fn router() -> axum::Router` — the single construction that
`spawn_mock` binds and that Task 8's standalone binary reuses, so the two can never drift
into answering differently.

Also create `rust/src/bin/mock_upstream.rs` in this step (cargo auto-discovers `src/bin/*.rs`
and names the binary after the file, so no manifest entry is needed). Tasks 5 and 8 both diff
against it, and the Python app can only reach a mock that is a separate process:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8600);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("mock upstream on http://127.0.0.1:{port}");
    axum::serve(listener, lomora::mock::router()).await?;
    Ok(())
}
```

Run it with `cd rust && cargo run --bin mock_upstream`, which listens on `127.0.0.1:8600`.

```rust
use axum::{routing::{get, post}, Json, Router};
use axum::response::IntoResponse;
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// Built once and shared by `spawn_mock` and `src/bin/mock_upstream.rs`. Do not duplicate
/// these route definitions in a second place — the parity run and the tests must exercise
/// identical upstream behaviour.
pub fn router() -> Router {
    Router::new()
        .route("/v1/models", get(|| async { Json(json!({
            "object": "list",
            "data": [{"id": "mock-a", "object": "model"}, {"id": "mock-b", "object": "model"}]
        }))}))
        .route("/v1/chat/completions", post(handle_completion))
}

pub async fn spawn_mock() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router()).await.unwrap(); });
    (format!("http://{addr}"), handle)
}

async fn handle_completion(body: String) -> axum::response::Response {
    let streaming = body.contains("\"stream\":true");
    if body.contains("BOOM") {
        return (axum::http::StatusCode::BAD_REQUEST, "upstream exploded").into_response();
    }
    if streaming {
        // "data: " split across two chunks: the scanner must buffer, not assume boundaries.
        let a: Vec<u8> = b"data: {\"choices\":[{\"delta\":{\"content\":\"h".to_vec();
        let b: Vec<u8> = b"ello}}]}\n\n".to_vec();
        let c: Vec<u8> = b"data: [DONE]\n\n".to_vec();
        let s = futures_util::stream::iter(
            vec![Ok::<_, axum::Error>(a), Ok(b), Ok(c)]);
        return axum::response::Response::new(axum::body::Body::from_stream(s));
    }
    Json(json!({"choices": [{"message": {"content": "hello"}}],
                "usage": {"completion_tokens": 20, "prompt_tokens": 5, "total_tokens": 25}})).into_response()
}
```

Do not add a `Query<...>` extractor to `handle_completion`: an OpenAI completion request
carries its parameters in the body, and `Query<Value>` fails to deserialise when the URL has
no query string, which would make every mock call error for the wrong reason.

- [ ] **Step 2: Write the failing models tests**

`rust/tests/models_endpoint.rs`:

```rust
mod support;
use support::{spawn_app, spawn_mock};
use serde_json::Value;

#[tokio::test]
async fn returns_array_of_ids_from_the_upstream() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap().json().await.unwrap();
    assert!(v.is_array(), "must be an array, got {v}");
    assert_eq!(v.as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect::<Vec<_>>(),
               vec!["mock-a", "mock-b"]);
}

#[tokio::test]
async fn unreachable_upstream_is_200_with_error_object_not_array() {
    let (base, _h) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/api/models?base_url=http://127.0.0.1:9")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert!(v.get("error").is_some(), "expected error object, got {v}");
}

#[tokio::test]
async fn entries_without_a_usable_id_are_dropped() {
    // mirrors: [m.get("id") for m in data.get("data", []) if m.get("id")]
    let (mock, _m) = support::spawn_mock_sparse().await;
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/models?base_url={mock}")).await.unwrap()
        .json().await.unwrap();
    assert_eq!(v.as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect::<Vec<_>>(),
               vec!["real-one"]);
}
```

`spawn_mock_sparse` is a second mock route added to `src/mock.rs` in this same step: it
serves `/v1/models` with `{"data":[{"id":"real-one"},{"object":"model"},
{"id":"","object":"model"},{"id":null,"object":"model"}]}` — one usable id, one missing,
one empty, one null — so the filter is exercised rather than merely asserted about. The
empty-string case matters: Python's `if m.get("id")` is falsy for `""`, hence the
`.filter(|s| !s.is_empty())` in the implementation.

- [ ] **Step 3: Run to verify they fail**

Run: `cd rust && cargo test --test models_endpoint`
Expected: FAIL — route missing, falls through to static handler.

- [ ] **Step 4: Implement the route**

`resolve_base` goes in `config.rs`:

```rust
pub fn resolve_base(cfg: &Config, explicit: Option<&str>) -> String {
    normalize_base(explicit.filter(|s| !s.trim().is_empty()).unwrap_or(&cfg.lmstudio_url))
}
```

`routes.rs`:

```rust
use axum::extract::Query;
use serde::Deserialize;
use crate::app::AppState;
use crate::config::resolve_base;
use crate::error::AppError;

#[derive(Deserialize)] pub struct ModelQuery { #[serde(default)] pub base_url: Option<String> }

pub async fn list_models(State(st): State<AppState>, Query(q): Query<ModelQuery>) -> Json<Value> {
    let base = resolve_base(&st.cfg, q.base_url.as_deref());
    match st.http.get(format!("{base}/v1/models")).timeout(std::time::Duration::from_secs(10))
        .send().await
    {
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        Ok(r) => {
            let status = r.status();
            if !status.is_success() {
                return Json(serde_json::json!({ "error": status.to_string() }));
            }
            match r.json::<Value>().await {
                Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
                Ok(body) => {
                    let ids: Vec<Value> = body.get("data").and_then(|d| d.as_array())
                        .map(|arr| arr.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str())
                            .filter(|s| !s.is_empty()).map(|s| Value::String(s.to_string())))
                            .flatten().collect())
                        .unwrap_or_default();
                    Json(Value::Array(ids))
                }
            }
        }
    }
}
```

Add `.route("/api/models", get(crate::routes::list_models))` in `app.rs`, and construct the shared client in `main.rs` and `spawn_app`:

```rust
let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
    .pool_max_idle_per_host(64).build().unwrap();
```

- [ ] **Step 5: Run to verify they pass**

Run: `cd rust && cargo test --test models_endpoint`
Expected: 3 passed.

- [ ] **Step 6: Verify against live Ollama and diff Python**

Run: `curl -s "localhost:8501/api/models?base_url=http://localhost:11434"` and the same on `:8502` with `PORT=8502 cargo run`.
Expected: both `["beam:cloud","gemma4:12b"]` in that order. This is the free live check — it loads no model.

- [ ] **Step 7: Commit**

```bash
git add rust
git commit -m "Add shared reqwest client and /api/models with live Ollama parity"
```

---

## Task 4: SQLite store and `GET`/`POST /api/profile`

**Files:**
- Create: `rust/src/store.rs`, `rust/tests/profile.rs`
- Modify: `rust/src/lib.rs`, `rust/src/app.rs` (`AppState` gains `db`), `rust/src/models.rs`, `rust/src/routes.rs`, `rust/src/main.rs`, `rust/tests/support/mod.rs`

**Interfaces:**
- Consumes: `Config.db_path`.
- Produces:
  - `Db(Arc<tokio::sync::Mutex<rusqlite::Connection>>)` — `Db::open(path: &Path) -> anyhow::Result<Db>`, `Db::default_profile() -> Db` (in-memory, for tests)
  - `struct RunRow { kind: String, timestamp: String, model: String, max_tokens: i64, ttft_ms: Option<f64>, total_s: f64, tokens: i64, tok_per_s: f64, concurrency: Option<i64>, base_url: Option<String> }`
  - `Db::insert_run(&self, r: &RunRow) -> anyhow::Result<()>`
  - `Db::recent_runs(&self, limit: i64) -> anyhow::Result<Vec<RunRow>>` — **oldest-first**
  - `Db::profile(&self) -> Profile`, `Db::set_profile(&self, name: Option<&str>, notes: Option<&str>) -> Profile`
  - `struct Profile { name: String, notes: String }`

- [ ] **Step 1: Write the failing profile tests**

`rust/tests/profile.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd rust && cargo test --test profile`
Expected: FAIL — no route.

- [ ] **Step 3: Write `store.rs`**

```rust
use std::path::Path;
use std::sync::Arc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct RunRow {
    pub kind: String, pub timestamp: String, pub model: String, pub max_tokens: i64,
    pub ttft_ms: Option<f64>, pub total_s: f64, pub tokens: i64, pub tok_per_s: f64,
    pub concurrency: Option<i64>, pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Profile { pub name: String, pub notes: String }

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('single','stress')),
  created_at TEXT NOT NULL, timestamp TEXT NOT NULL, model TEXT NOT NULL,
  max_tokens INTEGER NOT NULL, ttft_ms REAL, total_s REAL NOT NULL,
  tokens INTEGER NOT NULL, tok_per_s REAL NOT NULL, concurrency INTEGER, base_url TEXT);
CREATE TABLE IF NOT EXISTS profile (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  name TEXT NOT NULL DEFAULT 'My workstation', notes TEXT NOT NULL DEFAULT '');
INSERT OR IGNORE INTO profile (id, name, notes) VALUES (1, 'My workstation', '');";

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }
    pub fn default_profile() -> Db {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(SCHEMA).expect("schema");
        Db(Arc::new(Mutex::new(conn)))
    }
    pub async fn insert_run(&self, r: &RunRow) -> anyhow::Result<()> {
        let (kind, ts, model, mt, ttft, total, tok, tps, conc, base) = (
            r.kind.clone(), r.timestamp.clone(), r.model.clone(), r.max_tokens, r.ttft_ms,
            r.total_s, r.tokens, r.tok_per_s, r.concurrency, r.base_url.clone());
        let created = chrono::Local::now().to_rfc3339();
        let c = self.0.clone(); let g = c.lock().await;
        g.execute("INSERT INTO runs (kind, created_at, timestamp, model, max_tokens, ttft_ms, \
                   total_s, tokens, tok_per_s, concurrency, base_url) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![kind, created, ts, model, mt, ttft, total, tok, tps, conc, base])?;
        Ok(())
    }
    /// ORDER BY id DESC LIMIT n, then reversed: Python returns list(deque) oldest-first and
    /// index.html reverses it itself. Spec 5.1.
    pub async fn recent_runs(&self, limit: i64) -> anyhow::Result<Vec<RunRow>> {
        let c = self.0.clone(); let g = c.lock().await;
        let mut stmt = g.prepare("SELECT kind, timestamp, model, max_tokens, ttft_ms, total_s, \
                   tokens, tok_per_s, concurrency, base_url FROM runs ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |row| Ok(RunRow {
            kind: row.get(0)?, timestamp: row.get(1)?, model: row.get(2)?, max_tokens: row.get(3)?,
            ttft_ms: row.get(4)?, total_s: row.get(5)?, tokens: row.get(6)?, tok_per_s: row.get(7)?,
            concurrency: row.get(8)?, base_url: row.get(9)?,
        }))?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        out.reverse();
        Ok(out)
    }
    pub async fn profile(&self) -> Profile {
        let c = self.0.clone(); let g = c.lock().await;
        g.query_row("SELECT name, notes FROM profile WHERE id = 1", [], |r|
            Ok(Profile { name: r.get(0)?, notes: r.get(1)? }))
            .unwrap_or(Profile { name: "My workstation".into(), notes: String::new() })
    }
    pub async fn set_profile(&self, name: Option<&str>, notes: Option<&str>) -> Profile {
        let c = self.0.clone(); let g = c.lock().await;
        // Connection::execute takes &self, so the guard must not be `mut` — `unused_mut`
        // is a rustc warning and the build runs with `-D warnings`.
        if let Some(n) = name { let _ = g.execute("UPDATE profile SET name = ?1 WHERE id = 1", params![n]); }
        if let Some(v) = notes { let _ = g.execute("UPDATE profile SET notes = ?1 WHERE id = 1", params![v]); }
        drop(g);
        self.profile().await
    }
}
```

Add `chrono = { version = "0.4", features = ["serde"] }` to `Cargo.toml` — needed for the local `"HH:MM:SS"` timestamp format in later tasks and RFC 3339 here.

- [ ] **Step 4: Add the profile route and its response struct**

`models.rs` — declaration order is the JSON order, matching Python's dict at `main.py:163`:

```rust
#[derive(Serialize)]
pub struct ProfileOut {
    pub name: String, pub notes: String, pub cpu: String,
    pub cpu_cores_physical: Option<usize>, pub cpu_cores_logical: usize,
    pub ram_total_gb: f64, pub os: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu_vram_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu_driver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu_error: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ProfileReq { pub name: Option<String>, pub notes: Option<String> }
```

`routes.rs`:

```rust
pub async fn get_profile(State(st): State<AppState>) -> Json<Value> {
    let p = st.db.profile().await;
    let s = st.hw.snapshot().await;
    let st2 = &st.sys;   // captured once at startup; see the note below
    let out = ProfileOut {
        name: p.name, notes: p.notes, cpu: st2.cpu.clone(),
        cpu_cores_physical: st2.cores_physical,
        cpu_cores_logical: st2.cores_logical,
        ram_total_gb: s.ram_total_gb, os: st2.os.clone(), gpu: None, gpu_vram_gb: None,
        gpu_driver: None, gpu_error: None,
    };
    Json(serde_json::to_value(out).unwrap())
}
```

Do **not** build a `SystemInfo::new_all()` inside the handler. `refresh_all()` walks the
process table and is the same class of mistake as the blocking `psutil` call the port exists
to remove — the profile endpoint is polled alongside the hardware cards. Instead capture the
immutable facts once at startup into a `ProfileStatic` and put it on `AppState`:

```rust
// hw.rs
#[derive(Clone, Debug)]
pub struct ProfileStatic { pub cpu: String, pub cores_physical: Option<usize>,
                           pub cores_logical: usize, pub os: String }

pub fn read_profile_static() -> ProfileStatic {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();
    let cpu = sys.cpus().first().map(|c| c.brand().trim().to_string())
        .filter(|b| !b.is_empty()).unwrap_or_else(|| "Unknown CPU".to_string());
    // sysinfo 0.40 exposes physical_core_count as either an associated function or a
    // method on a prepared System. The form below is the associated function; if rustc
    // reports "no function ... on type" or "this is an instance method", change it to
    // `sys.physical_core_count()` and nothing else. Do not guess a third form.
    let cores_physical: Option<usize> = sysinfo::System::physical_core_count();
    ProfileStatic { cpu, cores_physical,
        cores_logical: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        // Python emits `f"{platform.system()} {platform.release()}"`; shape match only,
        // spec deviation 3.
        os: format!("{} {}", std::env::consts::OS,
                     sysinfo::System::os_version().unwrap_or_default())
            .trim().to_string() }
}
```

The `cpu_cores_logical >= cpu_cores_physical` assertion in the Task 4 test is what catches a
wrong-but-compiling answer here, so read the value against `wmic cpu get NumberOfCores` or
Task Manager once and confirm it before moving on.

Add `pub sys: ProfileStatic` to `AppState` and populate it in `AppState::new`.

```rust
pub async fn set_profile(State(st): State<AppState>, Json(req): Json<crate::models::ProfileReq>) -> Json<Value> {
    let p = st.db.set_profile(req.name.as_deref(), req.notes.as_deref()).await;
    Json(serde_json::json!({ "name": p.name, "notes": p.notes }))
}
```

Wire `pub db: Db` into `AppState`, register both routes, and give `main.rs` `Db::open(&cfg.db_path)?`. In `spawn_app`, use `Db::default_profile()` so each test gets an isolated database — no shared file, no cross-test interference.

The `gpu*` keys stay `None` in this task; Task 6 fills them from the `Gpu` trait, which is why `hw.rs` already declares `name`, `vram_gb`, and `driver`.

- [ ] **Step 5: Run to verify they pass**

Run: `cd rust && cargo test --test profile && cargo test --test scaffold`
Expected: all pass. `scaffold` must still pass — `spawn_app`'s signature changed, so update call sites if the compiler complains.

- [ ] **Step 6: Live diff against Python**

Run both servers, then `curl -s localhost:8501/api/profile` and `:8502`.
Expected: identical key sets and order. `name`, `notes`, `ram_total_gb`, `cpu_cores_*` match exactly; `cpu` and `os` differ in wording — deviations 2 and 3 in the spec, expected, not failures. Python's `gpu: "Quadro P5000"` will be absent on `:8502` until Task 6; note it and move on rather than half-implementing it now.

- [ ] **Step 7: Commit**

```bash
git add rust
git commit -m "Add SQLite store and /api/profile with Python key ordering"
```

---

## Task 5: SSE scanner and `POST /api/test`

**Files:**
- Modify: `rust/src/bench.rs`, `rust/src/routes.rs`, `rust/src/models.rs`, `rust/src/app.rs`
- Test: `rust/tests/test_endpoint.rs`

**Interfaces:**
- Consumes: `AppState.http`, `resolve_base`, `Db::insert_run`, `round_half_even`, `AppError`, `ValidationErrors`.
- Produces:
  - `SseScanner::default()`, `SseScanner::push(&mut self, chunk: &[u8]) -> bool` — `true` on the chunk containing the first accepted `data:` line
  - `run_test(req: &TestReq, cfg: &Config, http: &reqwest::Client) -> Result<Value, AppError>` — the whole probe, no Axum types
  - `TestOut` response struct with keys in Python's order
  - `routes::run_test(State<AppState>, Json<TestReq>) -> Response`

- [ ] **Step 1: Write the failing unit tests for the scanner — before any route code**

`rust/src/bench.rs`:

```rust
#[cfg(test)]
mod sse_tests {
    use super::SseScanner;

    #[test]
    fn a_complete_first_frame_fires_on_that_chunk() {
        let mut s = SseScanner::default();
        assert!(s.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n"));
        assert!(s.finished());
    }

    #[test]
    fn frame_split_mid_payload_fires_when_the_newline_arrives() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"data: {\"choices\""));
        assert!(!s.push(b": [{\"delta\": "));
        assert!(s.push(b"{\"content\":\"x\"}}]}\n"));
    }

    #[test]
    fn prefix_split_across_chunks_still_counts_as_one_line() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"dat"));
        assert!(s.push(b"a: hello\n"));
        assert!(!s.push(b"data: again\n")); // already fired, stays fired-once
    }

    #[test]
    fn done_line_is_skipped_and_scanning_continues() {
        // Python: `if line.startswith("data: ") and "[DONE]" not in line` then break.
        // A [DONE] line does NOT stop the loop, it is simply not accepted.
        let mut s = SseScanner::default();
        assert!(!s.push(b"data: [DONE]\n\n"));
        assert!(!s.finished());
        assert!(s.push(b"data: late\n\n"));
    }

    #[test]
    fn non_data_lines_are_ignored() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"event: message\nid: 1\n\n"));
        assert!(!s.push(b"data:ok\n\n")); // no space after colon: Python requires "data: "
        assert!(s.push(b"data: now\n\n"));
    }

    #[test]
    fn several_frames_in_one_chunk_fire_only_once() {
        let mut s = SseScanner::default();
        assert!(s.push(b"data: one\n\ndata: two\n\n"));
        assert!(!s.push(b"data: three\n\n"));
    }
}
```

Run them and keep them failing; do not adjust a single expectation to make a first
implementation pass. These six cases are the reason the port can claim the TTFT number is
right without loading a model.

- [ ] **Step 2: Run to verify they fail**

Run: `cd rust && cargo test --lib sse_tests`
Expected: compile error, `SseScanner` undefined.

- [ ] **Step 3: Implement the scanner and the percentile helper**

```rust
/// Incremental SSE line scanner. reqwest hands us arbitrary byte chunks; a Python
/// `aiter_lines()` hides that a "data: " line can straddle two of them. Buffers to
/// newlines and reproduces the Python predicate exactly.
#[derive(Default)]
pub struct SseScanner { buf: Vec<u8>, fired: bool }

impl SseScanner {
    pub fn push(&mut self, chunk: &[u8]) -> bool {
        if self.fired { return false; }
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.starts_with("data: ") && !line.contains("[DONE]") {
                self.fired = true;
                return true;
            }
        }
        false
    }
    pub fn finished(&self) -> bool { self.fired }
}

/// Python: s = sorted(l); idx = min(len-1, int(round(p*(len-1)))); returns None if empty.
/// The inner round is half-to-even, so this replicates the index rule rather than
/// delegating to a percentile crate. Spec 5.1.
pub fn percentile_at(sorted: &mut Vec<f64>, p: f64) -> Option<f64> {
    if sorted.is_empty() { return None; }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((sorted.len() as f64 - 1.0) * p).round_ties_even() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}
```

`f64::round_ties_even` is stable since 1.77 and is exactly Python's tie behaviour; use it instead of hand-rolled math.

- [ ] **Step 4: Add the response struct and validation**

`models.rs` — extend `TestReq` with defaults and add:

```rust
#[derive(Serialize)]
pub struct TestOut {
    pub timestamp: String, pub model: String, pub max_tokens: i64,
    pub ttft_ms: Option<f64>, pub total_s: f64, pub tokens: i64, pub tok_per_s: f64,
}

impl TestReq {
    /// FastAPI validates with ge/le before the handler runs -> HTTP 422.
    pub fn validate(&self) -> Result<(), crate::error::ValidationErrors> {
        if self.max_tokens < 1 || self.max_tokens > 2048 {
            return Err(crate::error::ValidationErrors::from_field(
                "max_tokens", "Input should be between 1 and 2048", "less_than_equal"));
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Write the failing endpoint tests**

`rust/tests/test_endpoint.rs`:

```rust
mod support;
use support::{spawn_app, spawn_mock, fresh_app};
use serde_json::{json, Value};

#[tokio::test]
async fn success_body_has_python_key_order_and_types() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let r = reqwest::Client::new().post(format!("{base}/api/test"))
        .json(&json!({"model":"mock-a","base_url":mock,"max_tokens":20})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, ["timestamp","model","max_tokens","ttft_ms","total_s","tokens","tok_per_s"]);
    assert_eq!(v["tokens"], 20);
    assert!(v["ttft_ms"].as_f64().unwrap() > 0.0);
    assert!(v["total_s"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn tok_per_s_is_consistent_with_tokens_and_total_s() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let v: Value = reqwest::Client::new().post(format!("{base}/api/test"))
        .json(&json!({"model":"mock-a","base_url":mock,"max_tokens":20})).send()
        .await.unwrap().json().await.unwrap();
    let derived = crate_consistency(v["tokens"].as_f64().unwrap(), v["total_s"].as_f64().unwrap());
    assert!((v["tok_per_s"].as_f64().unwrap() - derived).abs() < 0.15,
        "reported {} vs derived {derived}", v["tok_per_s"]);
}

fn crate_consistency(tokens: f64, total_s: f64) -> f64 { tokens / total_s }

#[tokio::test]
async fn upstream_400_becomes_200_with_inference_failed_prefix() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let v: Value = reqwest::Client::new().post(format!("{base}/api/test"))
        .json(&json!({"model":"mock-a","base_url":mock,"prompt":"BOOM","max_tokens":20}))
        .send().await.unwrap().json().await.unwrap();
    let e = v["error"].as_str().unwrap();
    assert!(e.starts_with("TTFT probe failed:") || e.starts_with("Inference failed:"), "{e}");
}

#[tokio::test]
async fn out_of_range_max_tokens_is_422_with_detail() {
    let (base, _h) = spawn_app().await;
    let r = reqwest::Client::new().post(format!("{base}/api/test"))
        .json(&json!({"max_tokens": 5000})).send().await.unwrap();
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["detail"][0]["loc"], json!(["body","max_tokens"]));
}

#[tokio::test]
async fn defaults_apply_when_fields_are_omitted() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let v: Value = reqwest::Client::new().post(format!("{base}/api/test"))
        .json(&json!({"base_url": mock})).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["model"], "local-model");
    assert_eq!(v["max_tokens"], 100);
}
```

Timed values are asserted structurally, never by exact number — spec section 7.

- [ ] **Step 6: Implement the probe in `routes.rs`**

```rust
use std::time::Instant;
use futures_util::StreamExt;
use crate::bench::{round_half_even, SseScanner};
use crate::models::{TestOut, TestReq};

pub async fn run_test(State(st): State<AppState>, Json(req): Json<TestReq>) -> axum::response::Response {
    if let Err(e) = req.validate() { return e.into_response(); }
    match probe(&req, &st).await {
        Ok(v) => {
            let _ = st.db.insert_run(&v.1).await;
            (axum::http::StatusCode::OK, Json(v.0)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn probe(req: &TestReq, st: &AppState) -> Result<(serde_json::Value, crate::store::RunRow), AppError> {
    let base = resolve_base(&st.cfg, req.base_url.as_deref());
    let payload = serde_json::json!({
        "model": req.model, "messages": [{"role":"user","content":req.prompt}],
        "max_tokens": req.max_tokens, "stream": true });

    // 1) TTFT: stream one token, timestamp the first accepted data line.
    let t0 = Instant::now();
    let mut scanner = SseScanner::default();
    let r = st.http.post(format!("{base}/v1/chat/completions"))
        .timeout(std::time::Duration::from_secs(60)).json(&payload).send().await
        .map_err(|e| AppError(format!("TTFT probe failed: {e}")))?;
    if r.status().as_u16() >= 400 {
        let code = r.status(); let body = r.text().await.unwrap_or_default();
        return Err(AppError(format!("TTFT probe failed: HTTP {code}: {body}")));
    }
    let mut stream = r.bytes_stream();
    let ttft_ms = loop {
        match stream.next().await {
            Some(Ok(chunk)) => if scanner.push(&chunk) { break Some(round_half_even(t0.elapsed().as_secs_f64() * 1000.0, 1)) },
            Some(Err(e)) => return Err(AppError(format!("TTFT probe failed: {e}"))),
            None => break None,
        }
    };

    // 2) Token rate: non-streaming, full max_tokens.
    let t1 = Instant::now();
    let payload2 = serde_json::json!({
        "model": req.model, "messages": [{"role":"user","content":req.prompt}],
        "max_tokens": req.max_tokens });
    let r2 = st.http.post(format!("{base}/v1/chat/completions"))
        .timeout(std::time::Duration::from_secs(300)).json(&payload2).send().await
        .map_err(|e| AppError(format!("Inference failed: {e}")))?;
    let elapsed = t1.elapsed().as_secs_f64();
    if r2.status().as_u16() >= 400 {
        let code = r2.status(); let text = r2.text().await.unwrap_or_default();
        return Err(AppError(format!("Inference failed: HTTP {code}: {text}")));
    }
    let body: serde_json::Value = r2.json().await
        .map_err(|e| AppError(format!("Inference failed: invalid JSON: {e}")))?;
    let tokens = body.get("usage").and_then(|u| u.get("completion_tokens"))
        .and_then(|c| c.as_i64()).unwrap_or(0);
    let tps = if elapsed > 0.0 { round_half_even(tokens as f64 / elapsed, 1) } else { 0.0 };

    let out = TestOut {
        timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        model: req.model.clone(), max_tokens: req.max_tokens, ttft_ms,
        total_s: round_half_even(elapsed, 2), tokens, tok_per_s: tps };
    let row = crate::store::RunRow {
        kind: "single".into(), timestamp: out.timestamp.clone(), model: out.model.clone(),
        max_tokens: out.max_tokens, ttft_ms: out.ttft_ms, total_s: out.total_s,
        tokens: out.tokens, tok_per_s: out.tok_per_s, concurrency: None, base_url: None };
    Ok((serde_json::to_value(out).unwrap(), row))
}
```

Add `.route("/api/test", post(crate::routes::run_test))`, replacing the Task 1 placeholder, and add `use axum::response::IntoResponse;` to `routes.rs`.

- [ ] **Step 7: Add `fresh_app` to the test support module**

`spawn_app` uses `Config` defaults, which point at a dead LM Studio. `fresh_app(mock_base)` builds an `AppState` whose `lmstudio_url` is the mock, so tests need no `base_url` juggling; keep the explicit `base_url` assertions anyway because they mirror real usage:

```rust
pub async fn fresh_app(mock: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut cfg = lomora::config::Config::from_env();
    cfg.lmstudio_url = mock.to_string();
    let state = lomora::app::AppState::new(cfg);
    let handle = tokio::spawn(async move { axum::serve(listener, lomora::app::build_router(state)).await.unwrap(); });
    (format!("http://{addr}"), handle)
}
```

Introduce `AppState::new(cfg: Config) -> AppState` in `app.rs` so `main.rs`, `spawn_app`, and `fresh_app` build the state identically instead of three copies drifting.

- [ ] **Step 8: Run everything**

Run: `cd rust && cargo test --lib sse_tests && cargo test --test test_endpoint && cargo test`
Expected: scanner tests pass first (they were written before the route), endpoint tests pass, whole suite green.

- [ ] **Step 9: Diff against Python with the same mock as upstream**

Start the Python app against the shared mock (`cargo run --bin mock_upstream` on `:8600`;
`python -m http.server` will not do, it cannot answer `/v1/chat/completions`). Point both apps at it: Python `curl -s localhost:8501/api/test -d '{"base_url":"http://localhost:8600",...}'`, Rust `:8502` with the same body.
Expected: identical key names, order, and types. Values for `ttft_ms`, `total_s`, `tok_per_s` differ because they are measured, not replayed.

- [ ] **Step 10: Commit**

```bash
git add rust
git commit -m "Add buffered SSE scanner and /api/test with half-to-even rounding"
```

---

## Task 6: Stress fan-out and `POST /api/stress`

**Files:**
- Modify: `rust/src/bench.rs`, `rust/src/models.rs`, `rust/src/routes.rs`, `rust/src/hw.rs`, `rust/tests/support/mod.rs`
- Test: `rust/tests/stress_endpoint.rs`

**Interfaces:**
- Consumes: `AppState.http`, `SseScanner` not needed here, `percentile_at`, `Db::insert_run`.
- Produces:
  - `StressAggregate` with `fn from_samples(latencies: &[f64], tokens: &[i64], errors: &[String], success: usize, requests: i64, concurrency: i64, wall: f64) -> StressAggregate`
  - `StressOut` response struct, keys in Python order
  - `stress_test(State<AppState>, Json<StressReq>) -> axum::response::Response`
  - `from_samples(...) -> StressAggregate`, a free function in `bench.rs`
  - `AppState.gpu: Arc<dyn Gpu>` — added in Step 6 for the profile GPU fields

- [ ] **Step 1: Write the failing aggregate tests first**

```rust
#[cfg(test)]
mod agg_tests {
    use super::{from_samples, StressAggregate};

    #[test]
    fn empty_latencies_yield_none_not_zero() {
        let a = from_samples(&[], &[], &["x".into()], 0, 3, 2, 1.5);
        assert!(a.avg_latency_s.is_none() && a.p95_latency_s.is_none());
        assert_eq!(a.agg_tok_per_s, 0.0);
        assert_eq!(a.req_per_s, 0.0);
    }

    #[test]
    fn single_sample_is_its_own_average_and_p95() {
        let a = from_samples(&[0.4], &[7], &[], 1, 1, 1, 0.8);
        assert_eq!(a.avg_latency_s, Some(0.4));
        assert_eq!(a.p95_latency_s, Some(0.4));
        assert_eq!(a.total_tokens, 7);
        assert_eq!(a.agg_tok_per_s, 8.8);
        assert_eq!(a.req_per_s, 1.25);
    }

    #[test]
    fn p95_index_matches_the_python_rule_on_a_small_set() {
        // len 4: idx = min(3, round_ties_even(0.95*3)=round(2.85)=3) -> sorted[3]
        let lat = vec![0.1, 0.2, 0.3, 0.9];
        let a = from_samples(&lat, &[1,1,1,1], &[], 4, 4, 4, 1.0);
        assert_eq!(a.p95_latency_s, Some(0.9));
        assert_eq!(a.avg_latency_s, Some(0.4));
    }

    #[test]
    fn error_samples_are_capped_at_five() {
        let errs: Vec<String> = (0..9).map(|i| format!("HTTP {i}")).collect();
        let a = from_samples(&[], &[], &errs, 0, 9, 1, 1.0);
        assert_eq!(a.errors, 9);
        assert_eq!(a.error_samples.len(), 5);
        assert_eq!(a.error_samples[0], "HTTP 0");
    }

    #[test]
    fn zero_wall_does_not_divide_by_zero() {
        let a = from_samples(&[0.1], &[10], &[], 1, 1, 1, 0.0);
        assert_eq!(a.agg_tok_per_s, 0.0);
        assert_eq!(a.req_per_s, 0.0);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd rust && cargo test --lib agg_tests`
Expected: compile error, `from_samples` undefined.

- [ ] **Step 3: Implement the aggregate**

Appended to `rust/src/bench.rs`, where `percentile_at` and `round_half_even` are already in
scope from Task 2 and Task 5 — no `use` lines needed inside the same module.

```rust
#[derive(Clone, Debug)]
pub struct StressAggregate {
    pub success: usize, pub errors: usize, pub error_samples: Vec<String>,
    pub wall_s: f64, pub avg_latency_s: Option<f64>, pub p95_latency_s: Option<f64>,
    pub total_tokens: i64, pub agg_tok_per_s: f64, pub req_per_s: f64,
}

pub fn from_samples(latencies: &[f64], tokens: &[i64], errors: &[String],
                    success: usize, requests: i64, concurrency: i64, wall: f64) -> StressAggregate {
    let total_tokens: i64 = tokens.iter().sum();
    let mut lat = latencies.to_vec();
    let avg = if lat.is_empty() { None } else { Some(round_half_even(lat.iter().sum::<f64>() / lat.len() as f64, 3)) };
    let p95 = percentile_at(&mut lat, 0.95).map(|v| round_half_even(v, 3));
    let agg = if wall > 0.0 { round_half_even(total_tokens as f64 / wall, 1) } else { 0.0 };
    let rps = if wall > 0.0 { round_half_even(success as f64 / wall, 2) } else { 0.0 };
    StressAggregate {
        success, errors: errors.len(), error_samples: errors.iter().take(5).cloned().collect(),
        wall_s: round_half_even(wall, 2), avg_latency_s: avg, p95_latency_s: p95,
        total_tokens, agg_tok_per_s: agg, req_per_s: rps,
    }
}
```

`requests` and `concurrency` are passed for signature completeness with the Python inputs and are asserted in the response body; drop the unused parameter warning with a leading underscore if clippy insists.

- [ ] **Step 4: Write the failing endpoint tests**

`rust/tests/stress_endpoint.rs`:

```rust
mod support;
use support::{fresh_app, spawn_mock};
use serde_json::{json, Value};

#[tokio::test]
async fn all_succeed_against_the_mock_and_keys_follow_python_order() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let v: Value = reqwest::Client::new().post(format!("{base}/api/stress"))
        .json(&json!({"model":"mock-a","base_url":mock,"max_tokens":8,"concurrency":4,"requests":12}))
        .send().await.unwrap().json().await.unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, ["model","base_url","concurrency","requests","success","errors",
        "error_samples","wall_s","avg_latency_s","p95_latency_s","total_tokens","agg_tok_per_s","req_per_s"]);
    assert_eq!(v["success"], 12);
    assert_eq!(v["errors"], 0);
    assert_eq!(v["total_tokens"], 96);
    assert_eq!(v["concurrency"], 4);
}

#[tokio::test]
async fn unreachable_upstream_returns_200_with_zero_success() {
    let (base, _h) = support::spawn_app().await;
    let r = reqwest::Client::new().post(format!("{base}/api/stress"))
        .json(&json!({"base_url":"http://127.0.0.1:9","concurrency":2,"requests":3,"max_tokens":4}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "stress has no error path");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["success"], 0);
    assert_eq!(v["errors"], 3);
    assert!(v["avg_latency_s"].is_null() && v["p95_latency_s"].is_null());
    assert_eq!(v["agg_tok_per_s"], 0);
    assert!(!v["error_samples"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn concurrency_cap_is_actually_enforced() {
    // mock counts simultaneous in-flight requests and reports the peak in a header
    let (mock, _m) = support::spawn_limited_mock(20).await;
    let (base, _h) = fresh_app(&mock).await;
    let v: Value = reqwest::Client::new().post(format!("{base}/api/stress"))
        .json(&json!({"base_url":mock,"concurrency":3,"requests":15,"max_tokens":8}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["success"], 15);
    assert!(support::peak_inflight(&mock).await <= 3, "semaphore not honoured");
}

#[tokio::test]
async fn validation_bounds_for_concurrency_and_requests() {
    let (base, _h) = support::spawn_app().await;
    let c = reqwest::Client::new();
    for body in [json!({"concurrency": 40}), json!({"requests": 900}), json!({"concurrency": 0})] {
        let mut b = body.clone();
        b.as_object_mut().unwrap().insert("max_tokens".into(), json!(8));
        let r = c.post(format!("{base}/api/stress")).json(&b).send().await.unwrap();
        assert_eq!(r.status(), 422, "{b} should be rejected");
    }
}
```

Add `spawn_limited_mock` and `peak_inflight` to `tests/support/mod.rs`: the mock holds an `Arc<AtomicUsize>` counting concurrent in-flight requests, exposes the max via `GET /__peak`, and sleeps 50 ms per completion so overlap is measurable.

- [ ] **Step 5: Implement the handler with a semaphore**

```rust
use tokio::sync::Semaphore;
use std::sync::Arc;

pub async fn stress_test(State(st): State<AppState>, Json(req): Json<StressReq>)
    -> axum::response::Response
{
    if let Err(e) = req.validate() { return e.into_response(); }   // HTTP 422, same as TestReq
    let base = resolve_base(&st.cfg, req.base_url.as_deref());
    let sem = Arc::new(Semaphore::new(req.concurrency as usize));
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..req.requests {
        let (sem, http, base, req) = (sem.clone(), st.http.clone(), base.clone(), req.clone());
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let t = Instant::now();
            let payload = serde_json::json!({ "model": req.model,
                "messages": [{"role":"user","content":req.prompt}], "max_tokens": req.max_tokens });
            match http.post(format!("{base}/v1/chat/completions"))
                    .timeout(std::time::Duration::from_secs(300)).json(&payload).send().await {
                Err(e) => Err(format!("{e}")),
                Ok(r) => {
                    let code = r.status().as_u16();
                    if code >= 400 { let b = r.text().await.unwrap_or_default(); return Err(format!("HTTP {code}")); }
                    match r.json::<serde_json::Value>().await {
                        Err(e) => Err(format!("{e}")),
                        Ok(body) => {
                            let tok = body.get("usage").and_then(|u| u.get("completion_tokens"))
                                .and_then(|c| c.as_i64()).unwrap_or(0);
                            Ok((t.elapsed().as_secs_f64(), tok))
                        }
                    }
                }
            }
        }));
    }
    let (mut latencies, mut tokens_list, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for h in handles {
        match h.await {
            Ok(Ok((el, tk))) => { latencies.push(el); tokens_list.push(tk); }
            Ok(Err(msg)) => errors.push(msg),
            Err(e) => errors.push(format!("task join: {e}")),
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let success = latencies.len();
    let agg = from_samples(&latencies, &tokens_list, &errors, success, req.requests, req.concurrency, wall);

    let out = StressOut {
        model: req.model.clone(), base_url: base.clone(),
        concurrency: req.concurrency, requests: req.requests,
        success: agg.success, errors: agg.errors, error_samples: agg.error_samples,
        wall_s: agg.wall_s, avg_latency_s: agg.avg_latency_s, p95_latency_s: agg.p95_latency_s,
        total_tokens: agg.total_tokens, agg_tok_per_s: agg.agg_tok_per_s, req_per_s: agg.req_per_s,
    };
    let row = RunRow {
        kind: "stress".into(),
        timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        model: format!("{} (stress x{})", req.model, req.concurrency),
        max_tokens: req.max_tokens,
        ttft_ms: None,
        total_s: out.wall_s,
        tokens: out.total_tokens,
        tok_per_s: out.agg_tok_per_s,
        concurrency: Some(req.concurrency),
        base_url: Some(base.clone()),
    };
    let _ = st.db.insert_run(&row).await;
    (axum::http::StatusCode::OK, Json(serde_json::to_value(&out).unwrap())).into_response()
}
```

The synthetic history row is deliberately `"<model> (stress x<concurrency>)"` with
`ttft_ms: null` and `tok_per_s` equal to `agg_tok_per_s`, matching `main.py:259-267`.

`StressOut` goes in `models.rs` with fields declared in exactly the Python order, and
`StressReq` gains the same `validate()` returning `Result<(), ValidationErrors>` that
`TestReq` has, checking `max_tokens` 1..=2048, `concurrency` 1..=32, `requests` 1..=500:

```rust
fn default_maxtok_stress() -> i64 { 64 }
fn default_concurrency() -> i64 { 4 }
fn default_requests() -> i64 { 20 }

#[derive(Deserialize, Clone)]
pub struct StressReq {
    #[serde(default = "default_model")]  pub model: String,
    #[serde(default = "default_prompt")] pub prompt: String,
    #[serde(default = "default_maxtok_stress")] pub max_tokens: i64,
    #[serde(default)]                    pub base_url: Option<String>,
    #[serde(default = "default_concurrency")] pub concurrency: i64,
    #[serde(default = "default_requests")]    pub requests: i64,
}

#[derive(Serialize)]
pub struct StressOut {
    pub model: String, pub base_url: String, pub concurrency: i64, pub requests: i64,
    pub success: usize, pub errors: usize, pub error_samples: Vec<String>,
    pub wall_s: f64, pub avg_latency_s: Option<f64>, pub p95_latency_s: Option<f64>,
    pub total_tokens: i64, pub agg_tok_per_s: f64, pub req_per_s: f64,
}
```

Register the route as `.route("/api/stress", post(crate::routes::stress_test))`.

- [ ] **Step 6: Fill the GPU profile keys, which Task 4 left null**

Add `pub gpu: Arc<dyn Gpu>` to `AppState`, then in `get_profile` populate `gpu`, `gpu_vram_gb`, `gpu_driver`, or `gpu_error`, and when the trait reports no device set `gpu: Some("None detected".into())` — the exact string Python uses at `main.py:182`. Insert them in the Python order: `gpu`, `gpu_vram_gb`, `gpu_driver`.

- [ ] **Step 7: Run everything**

Run: `cd rust && cargo test --lib && cargo test`
Expected: aggregate unit tests pass, stress endpoint tests pass, full suite green.

- [ ] **Step 8: Diff against Python**

`curl -s localhost:8501/api/stress -H 'content-type: application/json' -d '{"model":"mock-a","base_url":"http://localhost:8600","concurrency":2,"requests":4,"max_tokens":8}'` and the same on `:8502`.
Expected: same keys and types; `success`, `errors`, `total_tokens`, `concurrency`, `requests` equal exactly; timing values differ as measured.

- [ ] **Step 9: Commit**

```bash
git add rust
git commit -m "Add semaphore stress fan-out, aggregation, and GPU profile fields"
```

---

## Task 7: History from SQLite and restart persistence

**Files:**
- Modify: `rust/src/routes.rs` (`get_history`), `rust/tests/history_persistence.rs`

**Interfaces:**
- Consumes: `Db::recent_runs`.
- Produces: `routes::get_history(State<AppState>) -> Json<Value>` returning oldest-first rows with Python's seven keys; `RunRow::to_history_json() -> Value`.

- [ ] **Step 1: Write the failing persistence tests**

`rust/tests/history_persistence.rs`:

```rust
mod support;
use support::{fresh_app, spawn_mock, fresh_app_on_db};
use serde_json::{json, Value};

#[tokio::test]
async fn rows_appear_oldest_first_with_seven_python_keys() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let c = reqwest::Client::new();
    for i in 0..3 {
        c.post(format!("{base}/api/test")).json(&json!({"model":format!("m{i}"),"base_url":mock,"max_tokens":8}))
          .send().await.unwrap();
    }
    let v: Value = c.get(format!("{base}/api/history")).send().await.unwrap().json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["model"], "m0");
    assert_eq!(arr[2]["model"], "m2");
    assert_eq!(arr[0].as_object().unwrap().keys().map(|k| k.as_str()).collect::<Vec<_>>(),
               ["timestamp","model","max_tokens","ttft_ms","total_s","tokens","tok_per_s"]);
}

#[tokio::test]
async fn history_is_capped_at_sixty_oldest_of_them_being_kept_recent() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let c = reqwest::Client::new();
    for _ in 0..65 { c.post(format!("{base}/api/test"))
        .json(&json!({"model":"m","base_url":mock,"max_tokens":4})).send().await.unwrap(); }
    let v: Value = c.get(format!("{base}/api/history")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 60);
    assert_eq!(v.as_array().unwrap()[59]["model"], "m");
}

#[tokio::test]
async fn stress_rows_show_up_as_synthetic_entries() {
    let (mock, _m) = spawn_mock().await;
    let (base, _h) = fresh_app(&mock).await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/api/stress")).json(&json!({"base_url":mock,"model":"m","requests":2,"concurrency":2,"max_tokens":4}))
      .send().await.unwrap();
    let v: Value = c.get(format!("{base}/api/history")).send().await.unwrap().json().await.unwrap();
    let row = &v.as_array().unwrap()[0];
    assert_eq!(row["model"], "m (stress x2)");
    assert!(row["ttft_ms"].is_null());
}

#[tokio::test]
async fn rows_survive_a_restart_on_a_shared_db_path() {
    let (mock, _m) = spawn_mock().await;
    let dir = std::env::temp_dir().join(format!("lomora-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("h.db");
    std::env::set_var("LOMORA_DB", &db);
    {
        let (base, _h) = fresh_app_on_db(&mock, &db).await;
        reqwest::Client::new().post(format!("{base}/api/test"))
            .json(&json!({"model":"kept","base_url":mock,"max_tokens":8})).send().await.unwrap();
    }
    let (base2, _h2) = fresh_app_on_db(&mock, &db).await;
    let v: Value = reqwest::Client::new().get(format!("{base2}/api/history")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "run must survive restart");
    assert_eq!(v.as_array().unwrap()[0]["model"], "kept");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd rust && cargo test --test history_persistence`
Expected: FAIL — `get_history` still returns the Task 1 `[]` placeholder.

- [ ] **Step 3: Implement**

`store.rs` gains:

```rust
impl RunRow {
    /// Python history entries carry exactly these seven keys, in this order.
    pub fn to_history_json(&self) -> serde_json::Value {
        serde_json::json!({
            "timestamp": self.timestamp, "model": self.model, "max_tokens": self.max_tokens,
            "ttft_ms": self.ttft_ms, "total_s": self.total_s, "tokens": self.tokens,
            "tok_per_s": self.tok_per_s })
    }
}
```

`routes.rs`:

```rust
pub async fn get_history(State(st): State<AppState>) -> Json<Value> {
    let rows = st.db.recent_runs(60).await.unwrap_or_default();
    Json(Value::Array(rows.iter().map(|r| r.to_history_json()).collect()))
}
```

`tests/support/mod.rs` gains `fresh_app_on_db(mock, path)` that opens a real `Db` at `path` rather than `Db::default_profile()`, so the restart test can build two apps over one file.

- [ ] **Step 4: Run to verify they pass**

Run: `cd rust && cargo test --test history_persistence && cargo test`
Expected: all green, including the restart test.

- [ ] **Step 5: Manual restart check**

Run `cd rust && PORT=8502 LOMORA_DB=./t.db cargo run`, POST a test with an Ollama `base_url`, Ctrl-C, relaunch against the same `t.db`, then `curl -s localhost:8502/api/history | python -m json.tool`.
Expected: the row is still there. Then confirm the Python app on `:8501` returns `[]` after its own restart, which is the behaviour this port deliberately improves (spec deviation list item 1).

- [ ] **Step 6: Commit**

```bash
git add rust
git commit -m "Serve /api/history from SQLite with restart persistence"
```

---

## Task 8: Full parity sweep, live smoke, clippy, release, report

**Files:**
- Create: `rust/tests/parity.rs`, `docs/parity-report.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: everything.
- Produces: the evidence package that gates Task 9.

- [ ] **Step 1: Write the parity tests as `#[ignore]`d**

They require both live servers and are run manually, not in `cargo test`, because they cannot be hermetic. Compare parsed `serde_json::Value` so float formatting differences (`23.5` versus `23.50`) do not create false failures.

```rust
// rust/tests/parity.rs
// Run: PARITY=1 cargo test --test parity -- --ignored --nocapture
// Requires: Python on :8501, Rust on :8502, mock upstream on :8600.
use serde_json::Value;

fn py(p: &str) -> String { format!("http://127.0.0.1:8501{p}") }
fn rs(p: &str) -> String { format!("http://127.0.0.1:8502{p}") }

#[tokio::test]
#[ignore]
async fn keys_types_and_order_match_on_every_endpoint() {
    let c = reqwest::Client::new();
    let base = std::env::var("MOCK_BASE").unwrap_or("http://127.0.0.1:8600".into());
    let mut report = String::new();
    let mut cases: Vec<(String, String, Option<Value>)> = vec![
        ("/api/hardware".into(), "GET".into(), None),
        ("/api/history".into(),   "GET".into(), None),
        (format!("/api/models?base_url={base}"), "GET".into(), None),
        ("/api/profile".into(),   "GET".into(), None),
        ("/api/test".into(), "POST".into(),
            Some(serde_json::json!({"model":"mock-a","base_url":base,"max_tokens":20}))),
        ("/api/stress".into(), "POST".into(),
            Some(serde_json::json!({"model":"mock-a","base_url":base,"max_tokens":8,"concurrency":2,"requests":4}))),
    ];
    for (path, method, body) in cases.drain(..) {
        let (a, b) = match (method.as_str(), body) {
            ("GET", _) => (c.get(py(&path)).send().await.unwrap(), c.get(rs(&path)).send().await.unwrap()),
            (_, Some(j)) => (c.post(py(&path)).json(&j).send().await.unwrap(),
                             c.post(rs(&path)).json(&j).send().await.unwrap()),
            _ => unreachable!(),
        };
        assert_eq!(a.status(), b.status(), "{path} status differs");
        let (va, vb): (Value, Value) = (a.json().await.unwrap(), b.json().await.unwrap());
        let (ka, kb) = (key_path(&va, &mut Vec::new()), key_path(&vb, &mut Vec::new()));
        report.push_str(&format!("{path}: python {ka:?} rust {kb:?} {}\n",
            if ka == kb { "MATCH" } else { "DIFFER" }));
    }
    println!("{report}");
    std::fs::write("../docs/parity-report.md", report).ok();
}

fn key_path(v: &Value, acc: &mut Vec<String>) -> Vec<String> {
    match v {
        Value::Object(m) => { for (k, child) in m { acc.push(k.clone()); key_path(child, acc); } }
        Value::Array(a) => { if let Some(first) = a.first() { key_path(first, acc); } }
        _ => {}
    }
    acc.clone()
}
```

Also assert exact equality for the deterministic subset: `/api/models` body, `/api/profile` name/notes/cores/`ram_total_gb`, `/api/history` row count and ordering after identical scripted runs against the shared mock, and the 422 detail shape for out-of-range bodies.

- [ ] **Step 2: Start the mock as a standalone target and run the parity suite**

The mock binary already exists from Task 3 — start it with `cargo run --bin mock_upstream`.
Start Python with `python main.py` and Rust with `cd rust && PORT=8502 cargo run`, then `cd rust && cargo test --test parity -- --ignored --nocapture`.
Note that `.venv` in this repo is an empty scaffold with no pip; the dependency set
(`fastapi`, `psutil`, `httpx`, `pynvml`) is installed for the **system** `python` on this
machine, so use plain `python main.py` rather than `.venv/Scripts/python.exe`.
Expected: every endpoint reports MATCH on key/type/order; any DIFFER line is a Task 1-7 defect to fix before continuing, not to document.

- [ ] **Step 3: The one live model load**

`curl -s localhost:8502/api/test -H 'content-type: application/json' -d '{"model":"gemma4:12b","base_url":"http://localhost:11434","prompt":"Name three planets.","max_tokens":16}'`
Expected: HTTP 200 with all seven keys, `tokens` greater than 0, `ttft_ms` plausible for a cold 8 GB load, `tok_per_s` non-zero. Run the identical call against `:8501` and compare the key sets and the `tok_per_s` order of magnitude — same ballpark, not identical numbers.
If `tokens` is 0 on both, that is upstream behaviour with a missing `usage` block and it is recorded as spec deviation 6, not fixed unilaterally.

- [ ] **Step 4: Static UI checks**

`curl -sI localhost:8502/static/index.html` and `curl -sI localhost:8502/` — expected 200 with `content-type: text/html`. Then relaunch with the environment variable set — `LOMORA_STATIC=../static PORT=8502 cargo run` — and confirm the on-disk page serves byte-identical content to the embedded one; it is a variable on the process, not a query string. Unset it and restart for the remaining checks. Confirm the dashboard renders and every card populates.

- [ ] **Step 5: Lint, build, measure**

```bash
cd rust && cargo clippy --all-targets -- -D warnings && cargo build --release
ls -la target/release/lomora.exe
```

Measure resident memory of the Rust process versus the Python one (`powershell Get-Process | Where ProcessName -match 'lomora|python' | Select ProcessName,WorkingSet`), and cold start time for each. Write the numbers, the parity report summary, and the live smoke result into `docs/parity-report.md`.
Expected: a real measurement in the report. "Lighter than Python" is a claim until a number is written down.

- [ ] **Step 6: Update the README**

Replace the Python instructions in `README.md` with `cargo build --release`, `cargo run`, the environment-variable table, and the SQLite note. Keep the Python section until Task 9 removes it — a README that describes code still in the tree is accurate.

- [ ] **Step 7: Commit**

```bash
git add rust docs README.md
git commit -m "Add parity report, live Ollama smoke evidence, and Rust build docs"
```

---

## Task 9: Remove Python, promote the crate to the repo root

**Gate:** do not start unless Task 8 reports every endpoint MATCH, restart persistence is proven, clippy is clean, and the release build works. If any of those is missing, stop and fix rather than deleting the reference implementation.

**Files:**
- Delete: `main.py`, `requirements.txt`, `.venv/`, `__pycache__/`
- Move: `rust/Cargo.toml`, `rust/src/**`, `rust/tests/**` to the repository root
- Modify: `rust/src/assets.rs` (embed folder path), `.gitignore`

- [ ] **Step 1: Move the crate up one level**

```bash
git mv rust/Cargo.toml rust/src rust/tests .
git rm -r rust
```

Expected: `Cargo.toml`, `src/`, `tests/`, `static/`, `docs/`, `LICENSE` at the root; `rust/` gone.

- [ ] **Step 2: Fix the embed path**

In `src/assets.rs`, change `#[folder = "../static"]` to `#[folder = "static"]`. Verify by `cargo build` — rust-embed fails loudly at compile time on a bad folder, which is the check.

- [ ] **Step 3: Delete the Python implementation**

```bash
git rm main.py requirements.txt
rm -rf .venv __pycache__
```

- [ ] **Step 4: Update `.gitignore`**

Replace the Python entries with `target/`, `lomora.db`, `*.db`; drop `__pycache__/`, `*.pyc`, `.venv/`, `requirements.txt`-related lines. Keep `.env`.

- [ ] **Step 5: Verify the relocated crate**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo build --release && ./target/release/lomora.exe`
Expected: full suite green from the new root, server answers all eight endpoints on `:8501`, dashboard loads from `http://localhost:8501`.

- [ ] **Step 6: Rewrite README for Rust-only**

Remove the Python quick start and the "Optional NVIDIA GPU with drivers" wording in favour of the NVML-detected note, `LMSTUDIO_URL` / `OLLAMA_URL` / `PORT` / `LOMORA_DB` / `LOMORA_STATIC`, and the binary-size and memory figures measured in Task 8 Step 5.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "Remove Python implementation and promote Rust crate to repo root"
```

This lands alone so `git revert` restores the Python version with one command.

---

## Final verification

- [ ] `cargo test` — every non-ignored test green.
- [ ] `cargo test --test parity -- --ignored` — all endpoints MATCH.
- [ ] `cargo clippy --all-targets -- -D warnings` — clean.
- [ ] `cargo build --release` — succeeds; binary size and RSS recorded in `docs/parity-report.md`.
- [ ] Manual: dashboard loads from the embedded assets, Run Test and Stress Test complete against Ollama, profile saves and survives a restart, GPU cards populated.
- [ ] `git log --oneline` shows one commit per task, with the Python removal last and isolated.

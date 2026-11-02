# Lomora Rust port — design

Date: 2026-11-02
Status: approved in section by section review, pending written-spec review
Scope: rewrite the Lomora backend in Rust with full wire compatibility, then remove the Python implementation

## 1. Background and motivation

Lomora is a local-LLM benchmarking web app. The backend is a single 281-line FastAPI
file ([main.py](../../../main.py)) exposing eight endpoints; the frontend is one
vanilla-JS page ([static/index.html](../../../static/index.html), 409 lines, Chart.js
and Inter font from CDNs). Test history is a 60-entry `deque` held in memory, and the
system profile is a plain dict — both are lost on restart.

The stated problem is that Python is "too heavy duty": a Python install, a venv, a
`pip install` of five packages, an uvicorn process, and several hundred MB resident
before it serves a page. The reason this port is tractable is that the app is shallow —
no database, no auth, no background jobs, no framework-specific behaviour, and nothing
in it that depends on the Python ecosystem.

The honest framing of the benefit: Rust buys **distribution and measurement fidelity**,
not throughput. Every request spends its time waiting on an inference server that takes
seconds, so the model server is the bottleneck and always will be. What a Rust port
actually fixes is the two places where the current code corrupts its own measurements,
plus the packaging story.

## 2. Decisions taken

| Question | Decision |
| --- | --- |
| Faithfulness | Parity plus hygiene fixes. Same endpoints and JSON; latent defects corrected; no new features. |
| Persistence | SQLite replaces the in-memory `deque`, so runs and profile survive restart. |
| Frontend | `static/index.html` reused unchanged. |
| Layout | Build under `rust/` beside the Python while both can run; after verification, delete Python and promote the crate to the repo root. |
| Verification | Deterministic in-process mock upstream for tests, plus one live smoke call against Ollama `gemma4:12b`. |
| Python fate | Removed completely, as a separate revertible commit, only after the gate in section 8 passes. |
| Approach | Axum + Tokio, ported as vertical slices, with the pure measurement logic unit-tested inside its own slice before it is wired to a route. |

Rejected alternatives: strict 1:1 parity (keeps the restart-wipes-history problem and the
timing skew); an added rating/scoring layer (out of scope, doubles the design surface);
`actix-web` (actor and macro conventions with no observable benefit here); hand-rolled
`hyper` (two to three times the code to reinvent routing, CORS preflight, and static
serving).

## 3. Verified environment facts

All of these were checked on 2026-11-02 on this machine rather than assumed.

- `rustc 1.97.1`, `cargo 1.97.1`, `rustup 1.29.0`, host `x86_64-pc-windows-msvc`,
  edition 2024 available.
- No `cl.exe`, `cc`, or `gcc` on `PATH` from Git Bash, but **Visual Studio 2022 BuildTools
  is installed** with the VC x64 tools component. A probe `cargo build` succeeded in 1.7 s.
- `libsqlite3-sys` with the `bundled` feature compiled successfully (15.6 s); the `cc`
  crate located MSVC through `find-msvc-tools`. Bundled SQLite is therefore viable and no
  substitute store is needed.
- Crates.io is reachable.
- `C:\Windows\System32\nvml.dll` is present; `nvidia-smi` reports **two** Quadro P5000
  cards, 16384 MiB each, driver 582.67. The Python reads index 0 only, and the port keeps
  that behaviour.
- LM Studio is **not** running on `:1234` (connection failed). Ollama **is** running on
  `:11434` with `beam:cloud` (remote) and `gemma4:12b` (local, ~8 GB Q4_K_M GGUF).
- `GET :11434/v1/models` returns `{"object":"list","data":[{"id":"beam:cloud",...},
  {"id":"gemma4:12b",...}]}`, and the Python model-list logic extracts
  `['beam:cloud', 'gemma4:12b']` from it. This is the reference behaviour for `/api/models`.

Dependency versions confirmed to resolve today: `axum 0.8.9`, `tokio 1.54.1`,
`reqwest 0.13.5`, `tower-http 0.7.1`, `rusqlite 0.41.1`, `sysinfo 0.40.1`,
`nvml-wrapper 0.13.0`, `rust-embed 8.13.0`, `serde 1.0.229`, `serde_json 1.0.154`,
`futures-util 0.3.34`.

## 4. Architecture

One binary crate, `lomora`, at:

```
rust/
  Cargo.toml
  src/
    main.rs      env config -> AppState -> spawn sampler -> serve router
    config.rs    LMSTUDIO_URL, OLLAMA_URL, PORT, LOMORA_DB, LOMORA_STATIC
    state.rs     AppState { http, hw, db, cfg }  (cheap clones, no globals)
    models.rs    request/response structs; serde names match Python exactly
    routes.rs    eight handlers, thin, no logic
    bench.rs     pure: SSE first-token parse, latency/percentile aggregation
    hw.rs        hardware sampler: sysinfo + optional NVML, last-snapshot
    store.rs     rusqlite schema, run inserts, history query
    error.rs     AppError -> IntoResponse as HTTP 200 + {"error": ...}
```

Three boundaries are deliberate, each answering a specific defect or requirement.

`bench.rs` performs no I/O. It takes bytes and timings in and returns measurements out,
which is what makes the TTFT and percentile logic unit-testable against hand-written fake
streams. A benchmark tool that reports a plausible-but-wrong number is worse than one that
fails loudly, and these two functions are where such a number would come from.

`hw.rs` is a background task sampling CPU and RAM on a fixed tick and publishing the
latest snapshot. `/api/hardware` then reads already-measured data. This replaces
`psutil.cpu_percent(interval=0.1)` called inline inside an `async def` at
[main.py:61](../../../main.py), which blocks the event loop for 100 ms on every poll —
and the UI polls every 3 s, including while a stress test is measuring latencies. It also
replaces a fresh `httpx.AsyncClient` constructed per request inside the fan-out at
[main.py:212](../../../main.py), which charges connection setup to the latency being
recorded. In Rust, one shared `reqwest::Client` is used for every upstream call, with
per-request timeouts.

`error.rs` exists to reproduce Python's habit of answering HTTP 200 with an `{"error": ...}`
body. The UI reads `data.error` after calling `r.json()` unconditionally, so a by-the-book
4xx would break the dashboard silently. The only exception is request validation, see
section 5.

GPU access sits behind a small trait with a real NVML implementation and a no-op
implementation, so a machine without a working driver produces exactly the
`gpu_util`-absent response shape the frontend already handles. Database access goes
through a single `rusqlite` connection behind a `tokio::sync::Mutex`, not a pool: writes
are a handful of benchmark rows and pooling would be a dependency bought for nothing.

The UI is embedded with `rust-embed` by default. `LOMORA_STATIC`, when set, serves from
that directory instead so the page can be iterated on without recompiling. This keeps the
single-binary goal while preserving development convenience.

Configuration defaults, all overridable by environment variable: `PORT` 8501 bound on
`0.0.0.0` as uvicorn is today, `LMSTUDIO_URL` `http://localhost:1234`, `OLLAMA_URL`
`http://localhost:11434`, `LOMORA_DB` `lomora.db` in the working directory, `LOMORA_STATIC`
unset. During the slice plan the Rust server runs with `PORT=8502` so Python on `:8501`
stays up and the two can be diffed simultaneously.

One caveat surfaced during self-review: declaring `OLLAMA_URL` and actually honouring it
turn out to be different things while the frontend stays untouched. The unchanged
`index.html` always sends an explicit `base_url`, because the provider dropdown hardcodes
both URLs in the browser, so the server-side default is never consulted in practice. The
port therefore parses `OLLAMA_URL` and applies it exactly where Python applies
`LMSTUDIO_URL` — as the fallback when `base_url` is omitted — but no visible behaviour
change follows from it. Truly wiring the provider to configuration means removing those
two literals from the page, which is a frontend change and sits outside the agreed scope.
It is recorded as a follow-up in section 9, not claimed as a fix.

## 5. Endpoint contracts

Paths, methods, and JSON keys are unchanged so `index.html` needs no edit. `round` below
always means half-to-even, see 5.1. Timeouts mirror the Python: 60 s for the TTFT probe,
300 s for inference and each stress request, 10 s for the models list.

**`GET /api/hardware`** — 200. Keys `cpu_percent` (float), `ram_percent` (float),
`ram_used_gb` (2), `ram_total_gb` (2). When a GPU is present, also `gpu_util` (int),
`gpu_mem_mb` (int), `gpu_mem_tot` (int), `gpu_temp_c` (int). Absent GPU means those four
keys are omitted, not null.

**`GET /api/models?base_url=`** — 200 with a JSON array of model-id strings, or 200 with
`{"error": string}`. The frontend branches on `Array.isArray`, so both shapes must stay.
Omitted `base_url` falls back to `LMSTUDIO_URL`.

**`POST /api/test`** — body `model` (default `"local-model"`), `prompt` (default
`"Count from 1 to 20, one number per line."`), `max_tokens` (default 100, valid 1..=2048),
`base_url` (optional). Success: `timestamp` (`"HH:MM:SS"` local), `model`, `max_tokens`,
`ttft_ms` (1 or null), `total_s` (2), `tokens` (int), `tok_per_s` (1). Upstream failure:
200 `{"error": "TTFT probe failed: ..."}` or `{"error": "Inference failed: ..."}`.
Appends one `kind = 'single'` run row.

**`POST /api/stress`** — body as `/api/test` plus `concurrency` (default 4, valid 1..=32)
and `requests` (default 20, valid 1..=500). Success: `model`, `base_url`, `concurrency`,
`requests`, `success`, `errors`, `error_samples` (up to 5 strings), `wall_s` (2),
`avg_latency_s` (3 or null), `p95_latency_s` (3 or null), `total_tokens`,
`agg_tok_per_s` (1), `req_per_s` (2). **There is no error path**: an unreachable upstream
makes every request fail and must still return 200 with `success: 0`, `errors: N`,
`avg_latency_s: null`, `p95_latency_s: null`, and zero throughput, exactly as Python does.
Appends one synthetic `kind = 'stress'` run row whose `model` is
`"<model> (stress x<concurrency>)"`, with `ttft_ms: null` and `tok_per_s` equal to
`agg_tok_per_s`.

**`GET /api/history`** — 200, array of run objects in the `/api/test` result shape,
**oldest first**, at most 60 entries.

**`GET /api/profile`** — 200 with `name`, `notes`, `cpu`, `cpu_cores_physical`,
`cpu_cores_logical`, `ram_total_gb` (2), `os`, and GPU keys `gpu`, `gpu_vram_gb` (2),
`gpu_driver`; or `gpu: "None detected"` when no GPU; or `gpu_error` (string) when NVML is
present but the call fails.

**`POST /api/profile`** — body `{name?, notes?}`; only supplied fields are written;
200 with `{"name": ..., "notes": ...}`.

**`GET /`** — serves `static/index.html`. CORS allows all origins, methods, and headers,
as the Python does.

### 5.1 Parity rules

**Rounding.** Python's `round()` is round-half-to-even; Rust's `f64::round` is
round-half-away-from-zero. These disagree on exact ties. A half-to-even helper is used at
every point where Python rounds. The p95 index has the same trap: Python computes
`min(len-1, int(round(p*(len-1))))`, and that inner `round` is also half-to-even, so the
index rule is replicated rather than delegating to a percentile crate whose rounding
differs.

**Ordering.** Python returns `list(history)` oldest-first and the UI reverses it itself
(`h.slice().reverse()`). The query is therefore `ORDER BY id DESC LIMIT 60`, reversed in
Rust before serialising. Getting this backwards inverts the chart with no error anywhere.

**Validation is the one place that is not 200.** FastAPI rejects an out-of-range
`max_tokens`, `concurrency`, or `requests` with **HTTP 422** from pydantic before the
handler runs, with a `{"detail": [...]}` body. The port mirrors 422 and a `detail` body
rather than converting it into a friendly 200, because preserving wire behaviour is the
point. A malformed or non-JSON request body is the same case: FastAPI answers 422, and so
does the port. The current UI cannot reach either state.

**Timestamps.** `"HH:MM:SS"` in local time, same as `time.strftime("%H:%M:%S")`.

## 6. Persistence

Created on first launch at `LOMORA_DB`, default `lomora.db` in the working directory
(documented, and added to `.gitignore`). No platform data-dir dependency.

```sql
CREATE TABLE IF NOT EXISTS runs (
  id          INTEGER PRIMARY KEY,
  kind        TEXT NOT NULL CHECK (kind IN ('single','stress')),
  created_at  TEXT NOT NULL,          -- RFC 3339 local, for ordering and future analysis
  timestamp   TEXT NOT NULL,          -- "HH:MM:SS", the field the UI displays
  model       TEXT NOT NULL,
  max_tokens  INTEGER NOT NULL,
  ttft_ms     REAL,
  total_s     REAL NOT NULL,
  tokens      INTEGER NOT NULL,
  tok_per_s   REAL NOT NULL,
  concurrency INTEGER,
  base_url    TEXT
);
CREATE TABLE IF NOT EXISTS profile (
  id    INTEGER PRIMARY KEY CHECK (id = 1),
  name  TEXT NOT NULL DEFAULT 'My workstation',
  notes TEXT NOT NULL DEFAULT ''
);
```

Rows are never pruned; `/api/history` returns the most recent 60. The default profile name
and empty notes match the Python's initial dict.

## 7. Testing strategy

**Unit layer** — pure functions, no I/O. The SSE parser is fed hand-written byte chunks
that split a frame mid-line, split it mid-`data:` prefix, bundle several frames into one
chunk, and terminate with `[DONE]`. The aggregation functions are tested against empty
latencies, a single sample, and index edges. The rounding helper is tested on exact ties.

**Integration layer** — a mock OpenAI-compatible upstream built from an `axum` router on
an ephemeral port, started inside the test process, not an external script. Tests drive the
real client through it.

**What a parity diff can and cannot prove.** Deterministic properties are byte-compared
against fixtures captured from the running Python app: field names, types, key presence
and absence, ordering, error shapes, 422 bodies, history behaviour, and synthetic stress
rows. Non-deterministic measured values — `ttft_ms`, `total_s`, `tok_per_s`, latencies —
are checked for structure and internal consistency instead, for example that
`tokens / total_s` agrees with the reported `tok_per_s`. No claim of value equality is made
for timings; it would be false.

**Live layer** — one real smoke run against Ollama `gemma4:12b` with a small `max_tokens`,
confirming genuine SSE framing, `usage.completion_tokens`, and the GPU sampler under model
load. `GET /api/models` is additionally verified against live Ollama because it costs
nothing.

**Also run**: `cargo clippy` clean, `cargo build --release`, and a reported release binary
size plus resident memory measured against the Python app, since reduced weight is the
stated motivation and the number should be evidence rather than assertion.

## 8. Slice plan and removal gate

Each slice ends with `cargo test`, a live diff of the same request against Python on
`:8501` and Rust on `:8502`, and its own commit. Because LM Studio is not running on this
machine, every live upstream call in the slices passes an explicit `base_url` for Ollama on
`:11434`; nothing depends on `:1234` being up. Slices 1-4 and 7 make no inference calls at
all, and slices 5 and 6 are verified through the in-process mock, so `gemma4:12b` loads
exactly once: during the final live smoke call.

1. Scaffold — crate, config, `AppError` 200-with-error shape, embedded static serving, so
   the page loads in a browser.
2. `GET /api/hardware` and the sampler task.
3. `GET /api/models`, verified against live Ollama.
4. `GET`/`POST /api/profile` plus SQLite schema creation.
5. `POST /api/test` — `bench.rs` SSE tests written first, then the route.
6. `POST /api/stress` — aggregation tests written first, then the fan-out.
7. `GET /api/history` from SQLite, including a restart-and-still-there check.
8. Parity sweep across all eight endpoints, live `gemma4:12b` smoke call, clippy, release
   build, size and memory report, README rewrite.

**The Python removal is gated on all of section 7 being green**: eight endpoints diffed,
restart persistence proven, embedded UI confirmed to serve. It then lands as its own
commit that deletes `main.py`, `requirements.txt`, `.venv`, and `__pycache__`, promotes the
crate from `rust/` to the repo root, and updates `.gitignore`. Keeping it a separate commit
means `git revert` restores the Python version intact.

## 9. Known deviations

Deliberate, and covered by the hygiene-fix decision: history and profile persist across
restart; one shared HTTP client replaces per-request clients; the hardware read no longer
blocks. `OLLAMA_URL` is parsed and applied as the `base_url`-omitted fallback, where the
Python declares it at [main.py:16](../../../main.py) and never reads it at all, but see the
section 4 caveat — with an unchanged frontend this is correctness, not a visible fix.
Removing the two hardcoded provider URLs from `index.html` is the follow-up that would make
it one.

Unavoidable, and not to be reported as parity failures:

1. **Error detail text.** httpx and reqwest word the same failure differently. Leading
   prose (`"TTFT probe failed: "`, `"Inference failed: "`) and the JSON shape match; the
   trailing sentence does not.
2. **`profile.cpu` content.** Python uses `platform.processor()`, which on Windows returns a
   registry string like `"Intel64 Family 6 Model 154 Stepping 4, GenuineIntel"`. `sysinfo`
   returns a brand string. Both are strings; the text differs, and the brand string is the
   more useful of the two.
3. **`profile.os` composition.** Python emits `"<system> <release>"`; the Rust equivalent is
   assembled from a different source. Type and intent match.
4. **`cpu_percent` sampling window.** A background tick rather than an inline 100 ms wait,
   so a single instantaneous reading may differ from what Python would have reported at that
   moment. This is the intended fix. Note that `sysinfo` returns 0 on its first refresh, so
   the sampler discards its first tick.
5. **Multi-GPU.** Only GPU 0 is read, matching the Python, even though this machine has two.
6. **`usage.completion_tokens`.** Read the same way as Python with a default of 0. If an
   upstream omits `usage`, token counts and throughput come out 0 in both implementations —
   inherited behaviour, checked explicitly during the live smoke run.

## 10. Out of scope

No rating or scoring layer, no comparison view, no saved prompt suites, no new endpoints,
no authentication, no browser-facing SSE or streaming to the UI, no database migration
tooling, and no CI packaging. Chart.js and the Inter font stay on their current CDNs, so
offline behaviour is unchanged from today.

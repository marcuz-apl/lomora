# Lomora - a Local LLM Rating Toolbox

A web dashboard for benchmarking and monitoring local LLM servers — LM Studio, Ollama, Moderado Gateway, or any OpenAI-compatible endpoint. Run single benchmarks, fan out stress tests, and browse a dated run history that survives restarts. The dashboard, the API, and the SQLite store ship in a single Rust binary; no interpreter or runtime is required.

## Requirements

- Rust toolchain (stable; tested with 1.97)
- (Optional) NVIDIA GPU with a driver exposing NVML for GPU metrics

## Getting Started

1. Clone the repository

   ```bash
   git clone https://github.com/marcuz-apl/lomora.git
   cd lomora
   ```

2. Build and run

   ```bash
   cargo run --release
   ```

   The release executable is written to `target/release/lomora` (`lomora.exe` on Windows). It embeds the dashboard, so it runs on its own.

3. (Optional) Start LM Studio and load a model (default endpoint http://localhost:1234).

4. Open your browser and visit

   ```
   http://localhost:8501
   ```

Run history is stored in a local SQLite database (`data/lomora.db` by default, created on first start) and, like the system profile, survives restarts. The dashboard lists runs newest-first with full `YYYY-MM-DD HH:MM:SS` timestamps and pages through the whole dataset (10 rows per page) via `GET /api/history?limit=&offset=`, which reports the total count in `X-Total-Runs`. Each row carries a ✕ button that deletes exactly that record (`DELETE /api/history/{id}`). Rows written before the date became part of the timestamp are dated once at startup from their stored creation time.

### Benchmark question pool

The dashboard ships 18 built-in benchmark questions across five categories (reasoning, math, coding, knowledge, instruction). Three of them are the defaults behind the "Run 3 defaults" button, which fires them back-to-back as three separate history rows. A dropdown next to the prompt box picks any pool question, and "Fetch more" samples up to 8 questions per source from real benchmark datasets — GSM8K (math), HumanEval (coding, gzipped), TruthfulQA (knowledge) — caching them in SQLite and replacing the previously fetched set on each run. If every source is unreachable, the fetch reports an error and the existing pool stays untouched. Point `LOMORA_PROMPTS_URLS` at your own `category=url` sources to fetch from anything that serves JSONL, a JSON array, or their gzip variants, with `question`, `prompt`, or `text` as the text field.

### Providers and API keys

The Provider select presets the base URL: LM Studio (`:1234`), Ollama (`:11434`), Moderado Gateway (`:4788`), or Custom. The optional API Key field is kept only in the browser (localStorage) and sent inside benchmark request bodies; the server forwards it to the upstream as `Authorization: Bearer <key>` and never stores it.

### Stress test

The stress card fans `requests` completions (1-500) out behind a semaphore of `concurrency` permits (1-32) and records one synthetic history row named `<model> (stress xN)`. Its eight summary tiles sit in two rows of four (two per row on phones) and fade away 30 seconds after the run — the status box then says so, and the run itself stays in the history table and charts. TTFT is measured for single runs only: stress requests are non-streaming, so there is no first-token event to time.

### HTTP API

`GET /api/hardware`, `GET /api/models`, `GET|POST /api/profile`, `GET /api/history` (`?limit=&offset=`), `DELETE /api/history/{id}`, `POST /api/test`, `POST /api/stress`, `GET /api/prompts`, `POST /api/prompts/fetch`.

### Configuration

All settings come from environment variables:

| Variable        | Default                  | Purpose                                                          |
| --------------- | ------------------------ | ---------------------------------------------------------------- |
| `PORT`          | `8501`                   | HTTP port the server listens on.                                  |
| `LMSTUDIO_URL`  | `http://localhost:1234`  | LM Studio server used when a request has no `base_url` override.  |
| `OLLAMA_URL`    | `http://localhost:11434` | Fallback base URL when `LMSTUDIO_URL` is unset.                   |
| `LOMORA_DB`     | `data/lomora.db`         | SQLite file for run history and the saved profile (its folder is created when missing). |
| `LOMORA_STATIC` | *(embedded)*             | Serve the UI from this directory instead of the embedded copy.    |
| `LOMORA_PROMPTS_URLS` | *(built-in set)*   | Question-pool fetch sources as `category=url` pairs, comma-separated (e.g. `math=https://…/test.jsonl,coding=https://…/data.json.gz`). |

When a request carries no `base_url`, the base URL is resolved in this order: `LMSTUDIO_URL`, then `OLLAMA_URL`, then `http://localhost:1234`.

GPU metrics need an NVIDIA driver that exposes NVML (`nvidia-smi`). Without one the GPU counters are omitted and the profile reports `gpu: "None detected"`; every other feature keeps working.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

## Measured footprint

On the development machine (Windows, release build):

| Metric                     | Value     |
| -------------------------- | --------- |
| Release binary             | ~8.6 MiB  |
| Working set after startup  | 38-40 MB  |
| Cold start, first response | 3.1-3.6 s |

Startup is dominated by two full `sysinfo` system scans (one for the static profile snapshot, one for the hardware sampler).

## License

This project is licensed under the [MIT License](./LICENSE).

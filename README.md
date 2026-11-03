# Lomora - a Local LLM Rating Toolbox

A simple web app to monitor local LLM model performance (LM Studio compatible).

This is the Rust port of the original Python/FastAPI application: the same eight HTTP
endpoints, the same JSON shapes, and the unchanged `static/index.html` dashboard.

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

   The release executable is written to `target/release/lomora` (`lomora.exe` on Windows).
   It embeds the dashboard, so it runs on its own.

3. (Optional) Start LM Studio and load a model (default endpoint http://localhost:1234).

4. Open your browser and visit

   ```
   http://localhost:8501
   ```

Run history is stored in a local SQLite database (`lomora.db` by default). The dashboard
shows the most recent 60 runs, and both history and the system profile survive restarts.

### Benchmark question pool

The dashboard ships 18 built-in benchmark questions across five categories (reasoning,
math, coding, knowledge, instruction). Three of them are the defaults behind the
"Run 3 defaults" button, which fires them back-to-back as three separate history rows.
A dropdown next to the prompt box picks any pool question, and "Fetch more" samples up
to 8 questions per source from real benchmark datasets — GSM8K (math), HumanEval
(coding, gzipped), TruthfulQA (knowledge) — caching them in SQLite and replacing the
previously fetched set on each run. If every source is unreachable, the fetch reports an
error and the existing pool stays untouched. Point `LOMORA_PROMPTS_URLS` at your own
`category=url` sources to fetch from anything that serves JSONL, a JSON array, or their
gzip variants, with `question`, `prompt`, or `text` as the text field.

### Configuration

All settings come from environment variables:

| Variable        | Default                  | Purpose                                                          |
| --------------- | ------------------------ | ---------------------------------------------------------------- |
| `PORT`          | `8501`                   | HTTP port the server listens on.                                  |
| `LMSTUDIO_URL`  | `http://localhost:1234`  | LM Studio server used when a request has no `base_url` override.  |
| `OLLAMA_URL`    | `http://localhost:11434` | Fallback base URL when `LMSTUDIO_URL` is unset.                   |
| `LOMORA_DB`     | `lomora.db`              | SQLite file for run history and the saved profile.                |
| `LOMORA_STATIC` | *(embedded)*             | Serve the UI from this directory instead of the embedded copy.    |
| `LOMORA_PROMPTS_URLS` | *(built-in set)*   | Question-pool fetch sources as `category=url` pairs, comma-separated (e.g. `math=https://…/test.jsonl,coding=https://…/data.json.gz`). |

When a request carries no `base_url`, the base URL is resolved in this order:
`LMSTUDIO_URL`, then `OLLAMA_URL`, then `http://localhost:1234`.

GPU metrics need an NVIDIA driver that exposes NVML (`nvidia-smi`). Without one the GPU
fields report zeros and every other feature keeps working.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

`tests/parity.rs` is the ignored end-to-end comparison harness that was run against the
original Python server before its removal; its evidence is in
[`docs/parity-report.md`](docs/parity-report.md).

## Measured footprint

On the development machine (Windows, release build):

| Metric                       | This Rust server | Original Python server |
| ---------------------------- | ---------------- | ---------------------- |
| Release binary               | ~8.6 MiB         | n/a (interpreter)      |
| Working set after startup    | 38-40 MB         | 82 MB                  |
| Cold start, first response   | 3.1-3.6 s        | 9.4 s                  |

Startup is dominated by two full `sysinfo` system scans (one for the static profile
snapshot, one for the hardware sampler).

## License

This project is licensed under the [MIT License](./LICENSE).

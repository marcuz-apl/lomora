# Lomora - a Local LLM Rating Toolbox

A web dashboard for benchmarking and monitoring local LLM servers — LM Studio, Ollama, Moderado Gateway, or any OpenAI-compatible endpoint. Run single benchmarks, fan out stress tests, and browse a dated run history that survives restarts. The dashboard, the API, and the SQLite store ship in a single Rust binary; no interpreter or runtime is required.

## Download

Grab the binary for your platform from [Releases](https://github.com/marcuz-apl/lomora/releases/latest) and run it — no toolchain needed, the dashboard is embedded in the executable.

| Platform | Asset | First-run note |
|---|---|---|
| Windows x86_64 | `lomora-windows-x86_64.exe` | SmartScreen warns once about the unsigned binary: *More info* → *Run anyway*. |
| Linux x86_64 | `lomora-linux-x86_64` | `chmod +x lomora-linux-x86_64` before running. |
| macOS (Apple Silicon) | `lomora-macos-aarch64` | `chmod +x`, then right-click → *Open* the first time. |

Run it from a folder of its own — the database is created at `data/lomora.db` beside the working directory, and `PORT` / `LOMORA_DB` override the defaults — then open http://localhost:8501. GPU metrics need an NVIDIA driver exposing NVML (`nvml.dll` on Windows, `libnvidia-ml.so` on Linux); machines without one, including Apple Silicon Macs, report CPU and RAM only, and the dashboard explains why and links the driver download when a card is present.

## Build from source

Requirements: Rust stable (tested with 1.97), and optionally an NVIDIA driver exposing NVML for GPU metrics (a local LM Studio on the default endpoint http://localhost:1234 is optional too).

1. Clone the repository

   ```bash
   git clone https://github.com/marcuz-apl/lomora.git
   cd lomora
   ```

2. Build and run

   ```bash
   cargo run --release
   ```

   The release executable is written to `target/release/lomora` (`lomora.exe` on Windows) and embeds the dashboard, so it runs on its own. Open http://localhost:8501 once it starts.

Run history is stored in a local SQLite database (`data/lomora.db` by default, created on first start) and, like the system profile, survives restarts. The dashboard lists runs newest-first with full `YYYY-MM-DD HH:MM:SS` timestamps and pages through the whole dataset (10 rows per page) via `GET /api/history?limit=&offset=`, which reports the total count in `X-Total-Runs`. Each row carries a ✕ button that deletes exactly that record (`DELETE /api/history/{id}`). Rows written before the date became part of the timestamp are dated once at startup from their stored creation time.

### Benchmark question pool

The dashboard ships 18 built-in benchmark questions across five categories (reasoning, math, coding, knowledge, instruction). Three of them are the defaults behind the "Run 3 defaults" button, which fires them back-to-back as three separate history rows. A dropdown next to the prompt box picks any pool question, and "Fetch more" samples up to 8 questions per source from real benchmark datasets — GSM8K (math), HumanEval (coding, gzipped), TruthfulQA (knowledge) — caching them in SQLite and replacing the previously fetched set on each run. If every source is unreachable, the fetch reports an error and the existing pool stays untouched. Point `LOMORA_PROMPTS_URLS` at your own `category=url` sources to fetch from anything that serves JSONL, a JSON array, or their gzip variants, with `question`, `prompt`, or `text` as the text field.

### Providers and API keys

The Provider select presets the base URL: LM Studio (`:1234`), Ollama (`:11434`), Moderado Gateway (`:4788`), or Custom. The optional API Key field is kept only in the browser (localStorage) and sent inside benchmark request bodies; the server forwards it to the upstream as `Authorization: Bearer <key>` and never stores it.

### Stress test

The stress card fans `requests` completions (1-500) out behind a semaphore of `concurrency` permits (1-32) and records one synthetic history row named `<model> (stress xN)`. Its eight summary tiles sit in two rows of four (two per row on phones) and fade away 30 seconds after the run — the status box then says so, and the run itself stays in the history table and charts. TTFT is measured for single runs only: stress requests are non-streaming, so there is no first-token event to time.

### CRUD Toolbox (raw SQL)

The **CRUD Toolbox** button, left of `<- Prev 10` in the History header, opens a raw-SQL console beneath the history table. It is switched off unless the process environment says otherwise: start Lomora with `LOMORA_SQL=1` (or `true`, `on`, `yes`) to enable it. With the switch on, the console posts whatever you type straight to SQLite — no filtering, no preview, no undo. An `UPDATE` or `DELETE` rewrites or removes your recorded runs, a `DROP TABLE` destroys a table, and writes hold the database while they run, so benchmark requests queue behind them. Take a backup first: **Download backup** calls `GET /api/sql/backup`, which streams a consistent copy of the database (`VACUUM INTO`) as a dated `.db` file.

Scripts run statement by statement in the order written and stop at the first error. Each statement comes back with its columns and rows, its changed-row count, and a flag when a read was cut short at 200 rows; anything still running after 10 seconds is interrupted and the database stays usable. `Ctrl+Enter` (or `Cmd+Enter`) runs, and any statement that wrote rows refreshes the table and both charts, so what is on screen stays the truth. With the switch off, both endpoints answer the usual 200 error envelope saying the console is disabled and the panel reports that message — the routes exist in every build, so there is nothing for the UI to guess about.

### Console window and background running

On Windows a console opened by a double-click, an Explorer launch, or an autostart entry is Lomora's alone, so Lomora hides it as it starts and leaves only the dashboard. Started from a shell the console belongs to that shell, so it stays open — hiding it would hide your terminal — and Lomora prints a note instead. `--hidden` asks for the hidden window explicitly (on a shared console it prints that same note rather than pulling the window out from under the shell), and `--console` never hides, which is what you want when reading startup messages. If startup fails after the window was hidden, Lomora shows it again before printing the error.

On Linux and macOS `--hidden` means something different, because there is no window to hide: Lomora re-executes itself in a new session, waits until the dashboard answers on `PORT`, prints the address and the pid, and exits. The detached process writes its pid beside the database (`data/lomora.pid`), so `kill "$(cat data/lomora.pid)"` stops it. Without `--hidden` the process stays in the foreground, as before.

Options are refused rather than ignored: an unknown token prints the full usage and exits with status 2. `--help` prints the same usage and exits 0.

### HTTP API

`GET /api/hardware` (its `gpu_status` says why GPU counters are missing: `ok`, `driver_missing`, `no_nvidia`, `unsupported` or `unknown`), `GET /api/version`, `GET /api/models`, `GET|POST /api/profile`, `GET /api/history` (`?limit=&offset=`), `DELETE /api/history/{id}`, `POST /api/test`, `POST /api/stress`, `GET /api/prompts`, `POST /api/prompts/fetch`, `POST /api/sql` and `GET /api/sql/backup` (both gated on `LOMORA_SQL`).

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
| `LOMORA_SQL`    | *(off)*                  | Enables the CRUD Toolbox raw-SQL console (`POST /api/sql`, `GET /api/sql/backup`). Accepts `1`, `true`, `on`, `yes`; anything else, including the variable being unset, leaves it disabled. |
| `LOMORA_HIDDEN` | *(off)*                  | Same as `--hidden` (`1`, `true`, `on`, `yes`): hide the console window Lomora owns on Windows, start detached on Linux and macOS. |

When a request carries no `base_url`, the base URL is resolved in this order: `LMSTUDIO_URL`, then `OLLAMA_URL`, then `http://localhost:1234`.

GPU metrics need an NVIDIA driver that exposes NVML (`nvidia-smi`). Without one the GPU counters are omitted, the profile reports `gpu: "None detected"`, and the dashboard explains why: if a best-effort PCI/PNP probe still finds an NVIDIA card, the hint links the Windows and Linux driver downloads; otherwise it says no NVIDIA card is present, or that macOS is unsupported. Every other feature keeps working. NVML is re-detected every ten seconds while it is missing, so installing the driver and reloading the page is enough — no process restart. The CUDA toolkit is not needed: NVML ships with the display driver.

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

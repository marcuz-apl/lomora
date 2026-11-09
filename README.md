# Lomora - a Local LLM Rating Toolbox

A web dashboard for benchmarking and monitoring local LLM servers — LM Studio, Ollama, Moderado Gateway, or any OpenAI-compatible endpoint. Run single benchmarks, fan out stress tests, and browse a dated run history that survives restarts. The dashboard, the API, and the SQLite store ship in a single Rust binary; no interpreter or runtime is required.

## Install

### One line: curl (Linux, macOS) / irm (Windows)

```bash
curl -fsSL https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.ps1 | iex
```

Each installer detects the platform, downloads the matching binary from the [latest release](https://github.com/marcuz-apl/lomora/releases/latest), verifies it against the release's `SHA256SUMS` when one is published, and installs it without sudo into `~/.local/bin` (Unix) or `%LOCALAPPDATA%\lomora\bin` (Windows), adding that folder to the user PATH when needed. Releases published before the checksum job still install over HTTPS with a warning instead of failing.

Because a piped script cannot take arguments, use environment variables:

| Variable | Purpose |
|---|---|
| `LOMORA_VERSION=v0.1.11` | Pin a release instead of using the latest tag. |
| `LOMORA_INSTALL_DIR=DIR` | Choose the install directory. |
| `LOMORA_RELEASE_BASE=URL` | Fetch the assets and `SHA256SUMS` from a mirror. |
| `LOMORA_SKIP_VERIFY=1` | Skip checksum verification. |

### npm

```bash
npm install -g lomora
lomora
```

The [npm package](https://www.npmjs.com/package/lomora) is a ~6 kB launcher with no runtime dependencies: the first time you run `lomora` it downloads the matching release binary, checks its SHA-256, and places it beside the command. Node is needed only to install and launch it — the server that runs is the same standalone Rust binary. There is deliberately no `postinstall` hook, because npm 12 blocks lifecycle scripts by default; the launcher fetches the binary itself, so `npm install -g lomora` needs no `allowScripts` approval. For offline or proxied machines, download the asset by hand and run `LOMORA_BINARY=/path/to/lomora lomora`; see [npm/README.md](./npm/README.md) for the other first-run variables.

### Manual download

Grab the binary for your platform from [Releases](https://github.com/marcuz-apl/lomora/releases/latest) and run it — no toolchain needed, the dashboard is embedded in the executable. `SHA256SUMS` in the same release lists the checksums.

| Platform | Asset | First-run note |
|---|---|---|
| Windows x86_64 | `lomora-windows-x86_64.exe` | SmartScreen warns once about the unsigned binary: *More info* → *Run anyway*. |
| Linux x86_64 | `lomora-linux-x86_64` | `chmod +x lomora-linux-x86_64` before running. |
| macOS (Apple Silicon) | `lomora-macos-aarch64` | `chmod +x`, then right-click → *Open* the first time. |

Only those three platforms have prebuilt binaries; Intel Macs, Linux ARM64 and Windows ARM64 need [a source build](#build-from-source).

Run it from a folder of its own — the database is created at `data/lomora.db` beside the working directory, and `PORT` / `LOMORA_DB` override the defaults — then open http://localhost:8501. GPU metrics need an NVIDIA driver exposing NVML (`nvml.dll` on Windows, `libnvidia-ml.so` on Linux); machines without one, including Apple Silicon Macs, report CPU and RAM only, and the dashboard explains why and links the driver download when a card is present.

### About code signing

The installers sidestep the *warning*, not the *signature*. SmartScreen's app-reputation prompt is triggered for files carrying a Mark-of-the-Web (`Zone.Identifier`): browsers add it, while `curl`, `Invoke-WebRequest` and Node's HTTP client do not, and the binary is started from a terminal rather than through Explorer. So a curl/npm install normally runs without the "Windows protected your PC" dialog; npm adds one more layer in that the command npm launches is `node.exe`, which then spawns `lomora.exe`. On macOS the equivalent is `com.apple.quarantine`: browser downloads get it (hence the right-click → *Open* note above), curl/npm downloads usually do not.

The executable itself is still unsigned (`Get-AuthenticodeSignature` reports `NotSigned`) and has no publisher identity. `SHA256SUMS` proves the download matches the release, not who produced it. Defender's cloud reputation / block-at-first-sight, attack-surface-reduction rules, AppLocker, WDAC, Windows S mode and corporate device policies can still block an unsigned binary, and a copy downloaded with a browser *will* show the SmartScreen warning.

Code signing is deliberately out of scope for now. Lomora is a small, newly released tool, and its trust model is the GitHub Releases page over HTTPS plus `SHA256SUMS`, which both installers check. That gives download integrity and keeps the terminal install path free of prompts, at the cost of the browser-download warning and managed-device policies noted above. Signing can be added to the release workflow later without changing any of the installers.

If an install step fails, [docs/troubleshooting.md](./docs/troubleshooting.md) works from the message you actually see: `504` bursts from the release endpoint, npm's `allowScripts` block, the 0.1.8 "binary is missing" launcher, a stale `lomora` in the shell's command cache, and the glibc floor on older Linux distributions.

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

Run history is stored in a local SQLite database (`data/lomora.db` by default, created on first start) and, like the system profile, survives restarts. The dashboard lists runs newest-first with full `YYYY-MM-DD HH:MM:SS` timestamps and pages through the whole dataset (10 rows per page) via `GET /api/history?limit=&offset=`, which reports the total count in `X-Total-Runs`. Each row shows the provider it ran against and a ✕ button that deletes exactly that record (`DELETE /api/history/{id}`). The provider label is written from the run's upstream when the row is inserted, not derived at render time, so a history mixing LM Studio, Ollama and llama.cpp stays labelled after the select changes; rows from before the column existed are backfilled once from their stored base URL. Rows written before the date became part of the timestamp are dated once at startup from their stored creation time.

### Benchmark question pool

The dashboard ships 18 built-in benchmark questions across five categories (reasoning, math, coding, knowledge, instruction). Three of them are the defaults behind the "Run 3 defaults" button, which fires them back-to-back as three separate history rows. A dropdown next to the prompt box picks any pool question, and "Fetch more" samples up to 8 questions per source from real benchmark datasets — GSM8K (math), HumanEval (coding, gzipped), TruthfulQA (knowledge) — caching them in SQLite and replacing the previously fetched set on each run. If every source is unreachable, the fetch reports an error and the existing pool stays untouched. Point `LOMORA_PROMPTS_URLS` at your own `category=url` sources to fetch from anything that serves JSONL, a JSON array, or their gzip variants, with `question`, `prompt`, or `text` as the text field.

### Providers and API keys

The Provider select presets the base URL: llama.cpp (`:9931`), LM Studio (`:1234`), Ollama (`:11434`), Moderado (`:4788`), or Custom. The label is recorded with each run as a stored history column (`LM Studio`, `Ollama`, `Moderado`, `llama.cpp`, or `Custom`), so the table stays readable when the select moves on to another server. The optional API Key field is kept only in the browser (localStorage) and sent inside benchmark request bodies; the server forwards it to the upstream as `Authorization: Bearer <key>` and never stores it.

### Stress test

The stress card fans `requests` completions (1-500) out behind a semaphore of `concurrency` permits (1-32) and records one synthetic history row named `<model> (stress xN)`. Its eight summary tiles sit in two rows of four (two per row on phones) and fade away 30 seconds after the run — the status box then says so, and the run itself stays in the history table and charts. TTFT is measured for single runs only: stress requests are non-streaming, so there is no first-token event to time.

### CRUD Toolbox (SQL console)

The **CRUD Toolbox** button, left of `<- Prev 10` in the History header, opens a SQL console beneath the history table. It is switched off by default. On a windowless Windows launch the tray's **Settings → Allow SQL CRUD** item opens it for that session after a confirmation, because row-level SQL on a `0.0.0.0` listener reaches the whole network; everywhere else start Lomora with `LOMORA_SQL=1` (or `true`, `on`, `yes`) to enable it. With the switch on, the console posts whatever you type straight to SQLite — no preview and no undo. Rows are fair game: an `UPDATE` or `DELETE` rewrites or removes your recorded runs, and writes hold the database while they run, so benchmark requests queue behind them. The schema is guarded by SQLite's authorizer, which refuses every `DROP` (table, index, view or trigger, temporary included), `ALTER TABLE`, `ATTACH`/`DETACH` and a write to `PRAGMA writable_schema`; `SELECT`, `INSERT`, `UPDATE`, `DELETE` and `CREATE` still run. Take a backup first: **Download backup** calls `GET /api/sql/backup`, which streams a consistent copy of the database (`VACUUM INTO`) as a dated `.db` file.

Scripts run statement by statement in the order written and stop at the first error. Each statement comes back with its columns and rows, its changed-row count, and a flag when a read was cut short at 200 rows; anything still running after 10 seconds is interrupted and the database stays usable. `Ctrl+Enter` (or `Cmd+Enter`) runs, and any statement that wrote rows refreshes the table and both charts, so what is on screen stays the truth. With the switch off, both endpoints answer the usual 200 error envelope saying the console is disabled and the panel reports that message — the routes exist in every build, so there is nothing for the UI to guess about.

### Console window and background running

On Windows a console opened by a double-click, an Explorer launch, or an autostart entry is Lomora's alone, so Lomora detaches from it as it starts: the window and its taskbar entry disappear, leaving only the tray icon. Closing the console rather than hiding the window is deliberate: on Windows 11 the default terminal is often Windows Terminal, which hosts the console in its own window, so hiding the window would leave that host on the taskbar. Started from a shell the console belongs to that shell, so Lomora hands the work to a console-less background process instead: the prompt comes straight back, the tray icon is the way to the dashboard, and closing the terminal no longer takes the server with it. `--hidden` asks for that explicitly; `--console` is the escape hatch that never closes anything and keeps the server in the foreground with its log line and Ctrl+C, which is what you want when reading startup messages. If startup fails after the console was closed there is no window left to print to, so the error appears in a message box.

When the console is closed, Lomora also adds a notification-area icon, so the running server never becomes unreachable: the icon's tooltip shows the dashboard address, a one-time notification repeats it at startup, and double-clicking the icon or choosing **Open dashboard** opens it in the default browser. **Settings → Allow SQL CRUD** opens or closes the SQL console for this session only — a confirmation is shown before it opens, and a restart returns to whatever `LOMORA_SQL` said. **About** lists the product name, version, dashboard address, repository and license, and **Check for updates…** opens the GitHub release page. **Quit** stops the server gracefully (in-flight requests and database writes finish first). Only `--console` launches get no icon — the window and Ctrl+C are already there — since every other Windows launch ends up tray-only. On Linux and macOS `--hidden` is a detached process controlled through its pid file instead.

On Linux and macOS there is no console window for Lomora to close: the terminal tab belongs to whatever launched the binary, so it stays until that terminal is closed. `--hidden` is the windowless path there — Lomora re-executes itself in a new session, waits until the dashboard answers on `PORT`, prints the address and the pid, and exits. The detached process writes its pid beside the database (`data/lomora.pid`), so `kill "$(cat data/lomora.pid)"` stops it. For a menu launch with no terminal at all, point a desktop entry at the binary with `Terminal=false` (Linux) or run it from a LaunchAgent or app bundle (macOS); without `--hidden` the process stays in the foreground, as before.

Options are refused rather than ignored: an unknown token prints the full usage and exits with status 2. `--help` prints the same usage and exits 0.

### HTTP API

`GET /api/hardware` (its `gpu_status` says why GPU counters are missing: `ok`, `driver_missing`, `no_nvidia`, `unsupported` or `unknown`), `GET /api/version`, `GET /api/models`, `GET|POST /api/profile`, `GET /api/history` (`?limit=&offset=`), `DELETE /api/history/{id}`, `POST /api/test`, `POST /api/stress`, `GET /api/prompts`, `POST /api/prompts/fetch`, `POST /api/sql` and `GET /api/sql/backup` (both behind the console's switch, which the tray can flip for the session).

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
| `LOMORA_SQL`    | *(off)*                  | Starting state of the CRUD Toolbox SQL console (`POST /api/sql`, `GET /api/sql/backup`); **Settings → Allow SQL CRUD** in the tray toggles the same gate for the current session without a restart. Accepts `1`, `true`, `on`, `yes`; anything else, including the variable being unset, leaves it disabled. |
| `LOMORA_HIDDEN` | *(off)*                  | Same as `--hidden` (`1`, `true`, `on`, `yes`): tray-only on Windows (already the default from a shell), detached on Linux and macOS. |

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
| Release binary             | ~9.1 MiB  |
| Working set after startup  | 38-40 MB  |
| Cold start, first response | 3.1-3.6 s |

Startup is dominated by two full `sysinfo` system scans (one for the static profile snapshot, one for the hardware sampler).

## License

This project is licensed under the [MIT License](./LICENSE).

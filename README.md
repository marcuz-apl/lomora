# Lomora - a Local LLM Rating Toolbox

A simple web app to monitor local LLM model performance (LM Studio compatible).

## Requirements

- Rust toolchain (stable; MSVC or GNU toolchain on Windows)
- (Optional) NVIDIA GPU with drivers for GPU metrics

## Getting Started (Rust)

1. Clone the repository

   ```bash
   git clone https://github.com/marcuz-apl/lomora.git
   cd lomora
   ```

2. Build and run the server (from the `rust/` crate directory)

   ```bash
   cd rust
   cargo run --release
   ```

   For development builds use `cargo run`; the release binary is written to
   `rust/target/release/lomora(.exe)`.

3. (Optional) Start LM Studio and load a model (default endpoint http://localhost:1234).

4. Open your browser and visit

   ```
   http://localhost:8501
   ```

The web UI (`static/index.html`) is embedded in the binary at build time, so the
release executable runs on its own. Runs are persisted in a local SQLite database
(default `lomora.db` in the working directory) and the dashboard history survives
restarts (the most recent 60 runs are shown).

### Configuration

All settings come from environment variables:

| Variable       | Default                 | Purpose                                                        |
| -------------- | ----------------------- | -------------------------------------------------------------- |
| `PORT`         | `8501`                  | HTTP port the server listens on.                                |
| `LMSTUDIO_URL` | `http://localhost:1234` | LM Studio server used when a request has no `base_url` override. |
| `OLLAMA_URL`   | `http://localhost:11434` | Fallback when `LMSTUDIO_URL` is unset.                          |
| `LOMORA_DB`    | `lomora.db`             | SQLite file for run history.                                    |
| `LOMORA_STATIC`| *(embedded)*            | Serve the UI from this directory instead of the embedded copy.  |

GPU metrics need an NVIDIA driver with NVML (`nvidia-smi`); without one the GPU
fields report zeros and everything else keeps working.

## Legacy Python version

The original Python implementation is still present in the repository root for
reference and is scheduled for removal.

- Install dependencies: `pip install -r requirements.txt`
- Run: `python main.py`
- Visit: `http://localhost:8501`

## License

This project is licensed under the [MIT License](./LICENSE).

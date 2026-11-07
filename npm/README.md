# lomora

A local LLM rating toolbox: a benchmarking and monitoring dashboard for LM Studio, Ollama, llama.cpp, Moderado, or any OpenAI-compatible endpoint. The dashboard, HTTP API and SQLite history ship in a single Rust binary — Node is needed only to install and launch it, not to run the server.

This npm package is a thin launcher. On install it downloads the matching binary from [GitHub Releases](https://github.com/marcuz-apl/lomora/releases/latest), verifies its SHA-256 when the release publishes `SHA256SUMS`, and places it beside the `lomora` command.

## Install

```bash
npm install -g lomora
lomora
```

Then open http://localhost:8501.

Run it from a folder of its own: the database is created at `data/lomora.db` relative to the working directory.

```bash
mkdir -p ~/lomora && cd ~/lomora
PORT=9000 lomora          # LOMORA_DB=/path/to/lomora.db also works
lomora --help
```

Prefer no Node at all? Use the [curl installer](https://github.com/marcuz-apl/lomora#install) instead.

## Supported platforms

| Platform | Binary |
|---|---|
| Linux x86_64 | `lomora-linux-x86_64` |
| macOS Apple Silicon | `lomora-macos-aarch64` |
| Windows x86_64 | `lomora-windows-x86_64.exe` |

Intel Macs, Linux ARM64 and Windows ARM64 have no prebuilt binary yet; build from source with `cargo build --release`.

## Install-time options

| Variable | Purpose |
|---|---|
| `LOMORA_BINARY` | Path to an existing binary to copy instead of downloading (offline / air-gapped installs). |
| `LOMORA_VERSION` | Release tag to fetch, e.g. `0.1.7`. Defaults to this package's version. |
| `LOMORA_RELEASE_BASE` | Base URL of a mirror serving the release assets and, ideally, `SHA256SUMS`. |
| `LOMORA_SKIP_DOWNLOAD=1` | Do not download anything (you provide the binary yourself). |
| `LOMORA_SKIP_VERIFY=1` | Skip checksum verification. |

If the download fails (proxy, blocked GitHub access), download the asset manually and run:

```bash
LOMORA_BINARY=/path/to/lomora npm rebuild lomora
```

`npm install --ignore-scripts` skips the download entirely; the command then explains how to recover.

## Notes

- The binaries are unsigned (Windows `Get-AuthenticodeSignature` reports `NotSigned`). The postinstall download has no Mark-of-the-Web and the binary is launched from a terminal, so the SmartScreen "Windows protected your PC" prompt normally does not appear; a copy downloaded in a browser does carry MOTW and will warn once (*More info → Run anyway*). Corporate WDAC/AppLocker policies can still require a signature.
- On macOS, browser downloads get `com.apple.quarantine` and may need right-click → *Open* the first time; curl and npm downloads are normally not quarantined.
- GPU metrics require an NVIDIA driver exposing NVML. Without one, CPU and RAM metrics still work.
- See the [project README](https://github.com/marcuz-apl/lomora#readme) for the HTTP API, configuration and the full feature list.
- MIT licensed.

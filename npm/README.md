# lomora

A local LLM rating toolbox: a benchmarking and monitoring dashboard for LM Studio, Ollama, llama.cpp, Moderado, or any OpenAI-compatible endpoint. The dashboard, HTTP API and SQLite history ship in a single Rust binary — Node is needed only to install and launch it, not to run the server.

This npm package is a thin launcher. On first run it downloads the matching binary from [GitHub Releases](https://github.com/marcuz-apl/lomora/releases/latest), verifies its SHA-256 when the release publishes `SHA256SUMS`, and places it beside the `lomora` command (or in a per-user cache directory when that location is read-only). No lifecycle script is involved, so npm's `allowScripts` policy does not need to be relaxed — `npm install -g lomora` works on npm 12 with its default settings.

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

## First-run options

These are read when the launcher fetches the binary (the first time you run `lomora`), and also when you run `node scripts/install.js` by hand.

| Variable | Purpose |
|---|---|
| `LOMORA_BINARY` | Path to an existing binary to copy instead of downloading (offline / air-gapped installs). |
| `LOMORA_VERSION` | Release tag to fetch, e.g. `0.1.8`. Defaults to this package's version. |
| `LOMORA_RELEASE_BASE` | Base URL of a mirror serving the release assets and, ideally, `SHA256SUMS`. |
| `LOMORA_CACHE_DIR` | Where to store the binary when the package's `bin` directory is not writable. |
| `LOMORA_SKIP_DOWNLOAD=1` | Do not download anything (you provide the binary yourself). |
| `LOMORA_SKIP_VERIFY=1` | Skip checksum verification. |

If the download fails (proxy, blocked GitHub access), fetch the asset manually and run the launcher with `LOMORA_BINARY` pointing at it:

```bash
LOMORA_BINARY=/path/to/lomora lomora
```

```powershell
$env:LOMORA_BINARY = 'C:\path\to\lomora.exe'; lomora
```

You can also invoke the downloader directly, which bypasses npm entirely:

```bash
node "$(npm root -g)/lomora/scripts/install.js"
```

## Notes

- The binaries are unsigned (Windows `Get-AuthenticodeSignature` reports `NotSigned`). The first-run download has no Mark-of-the-Web and the binary is launched from a terminal, so the SmartScreen "Windows protected your PC" prompt normally does not appear; a copy downloaded in a browser does carry MOTW and will warn once (*More info → Run anyway*). Corporate WDAC/AppLocker policies can still require a signature.
- On macOS, browser downloads get `com.apple.quarantine` and may need right-click → *Open* the first time; curl and npm downloads are normally not quarantined.
- GPU metrics require an NVIDIA driver exposing NVML. Without one, CPU and RAM metrics still work.
- See the [project README](https://github.com/marcuz-apl/lomora#readme) for the HTTP API, configuration and the full feature list.
- MIT licensed.

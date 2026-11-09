# Troubleshooting

Every entry starts from the message you actually see. Both installers are plain scripts — read them
in the repository, or with `curl -fsSL <url> | less` — so they are the ground truth for what an
install does.

Two commands answer most "which one am I running" questions:

```bash
type -a lomora                       # every lomora on PATH, in the order bash picks one
npm ls -g --depth=0 lomora           # the npm package version, if installed that way
curl -s http://localhost:8501/api/version   # the version of a server that is already running
```

## Install and download

### `curl: (22) The requested URL returned error: 504`

GitHub's release-download endpoint (`github.com/.../releases/download/...`) answers **504 in
bursts** for these assets. Measured on 2026-11-09: five failures in ten attempts, minutes apart,
for the same file, on a network that had just downloaded it twice. A 504 there says nothing about
the release and nothing about your machine.

`install.sh` and `install.ps1` handle this: five attempts two seconds apart, then a fallback to the
API asset endpoint (`api.github.com/repos/.../releases/assets/<id>` with
`Accept: application/octet-stream`), which serves the same bytes over a different path. You will see
one or more `504` lines and then `the release endpoint is failing; trying the API instead`. That is
the fallback working, not a failure.

If it still gives up, take the API route by hand — it bypasses the flaky endpoint entirely:

```bash
# GitHub CLI, if you have it
gh release download --repo marcuz-apl/lomora --pattern 'lomora-linux-x86_64'
```

Without `gh`, open the [latest release](https://github.com/marcuz-apl/lomora/releases/latest) and
click the asset. If even that page will not load, this fetches the same bytes through the API using
only `python3`:

```bash
python3 - <<'PY'
import json, urllib.request
def get(url, accept=None):
    h = {'User-Agent': 'lomora-docs'}
    if accept: h['Accept'] = accept
    return urllib.request.urlopen(urllib.request.Request(url, headers=h))
rel = json.load(get('https://api.github.com/repos/marcuz-apl/lomora/releases/latest'))
asset = next(a for a in rel['assets'] if a['name'] == 'lomora-linux-x86_64')
data = get(asset['url'], 'application/octet-stream').read()
open('lomora', 'wb').write(data)
print(f"wrote lomora, {len(data)} bytes")
PY
chmod +x lomora
```

Asset ids differ per release, which is why the snippet above resolves the id instead of hard-coding
one.

`LOMORA_RELEASE_BASE=URL` points both installers at a mirror of the release; when it is set the API
fallback is skipped, because a mirror is the whole answer.

### `checksum mismatch for lomora-linux-x86_64`

The bytes on disk do not match the release's `SHA256SUMS`. Either the download was truncated by a
dropped connection, or whatever served it (a proxy, a mirror) altered it. Delete the file and run
the installer again; do not bypass the check with `LOMORA_SKIP_VERIFY=1` to get past this.

Releases older than the checksum job have no `SHA256SUMS`. There the installer says
`verified by HTTPS only` and continues — that is expected, not a downgrade you need to fix.

## npm

### `install scripts blocked because they are not covered by allowScripts`

npm 12 blocks lifecycle scripts by default. For lomora this is **harmless and expected**: the
package has deliberately had no `postinstall` since 0.1.9. Do not reach for `--allow-scripts`,
`npm install-scripts approve` or `npm rebuild` — there is no script to approve or run.

On **0.1.8** the warning mattered, because that version relied on `postinstall` to place the
binary. It never ran, so the install left a launcher with nothing to launch. That is the next entry.

### `lomora: the lomora binary is missing from /usr/lib/node_modules/lomora/bin`

That message is the **0.1.8** launcher, word for word; older builds print their own variant. 0.1.9
and later fetch the binary on first run instead, so they cannot produce it. Confirm and move on:

```bash
npm ls -g --depth=0 lomora        # expect 0.1.8 or older
sudo npm install -g lomora@latest
```

`npm rebuild lomora` does **not** help here, despite what the 0.1.8 message says: rebuild only
re-runs lifecycle scripts, and under npm 12 it will not run them either. If you would rather not
use npm at all, remove the package and use the curl install:

```bash
sudo npm uninstall -g lomora
curl -fsSL https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.sh | sh
```

### `bash: /usr/bin/lomora: No such file or directory`

Not a missing file — a **stale bash hash entry**. Bash caches the full path of every command it has
run. On Debian and Ubuntu npm puts the command at `/usr/bin/lomora` (a symlink into
`/usr/lib/node_modules/lomora/bin/`); uninstalling the package deletes that symlink, but the current
shell keeps trying the remembered path and never re-searches `PATH`.

```bash
hash -r
type -a lomora
```

`hash -r` empties the cache. The same applies to a moved or replaced binary in any shell that has
already run it.

### `lomora: command not found` after the curl install

`~/.local/bin` is not on `PATH` yet. The installer prints the exact line when this happens:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
export PATH="$HOME/.local/bin:$PATH"
```

Windows installs go to `%LOCALAPPDATA%\lomora\bin`, which `install.ps1` appends to the *user* PATH —
a terminal opened before the install will not see it, so open a new one.

### Where the npm launcher puts the binary

Beside the launcher (`.../node_modules/lomora/bin/`) when that directory is writable, otherwise in a
per-user cache: `~/.cache/lomora` (`$XDG_CACHE_HOME/lomora`) or `%LOCALAPPDATA%\lomora` on Windows.
A global install owned by root therefore still works for an ordinary user, without sudo, on first
run. For an offline or proxied machine, skip the download:

```bash
LOMORA_BINARY=/path/to/lomora lomora
```

## Running

### Linux: `version 'GLIBC_2.39' not found`

The 0.1.9 and 0.1.10 Linux assets were built on `ubuntu-latest` and required glibc 2.39, so they
refused to load on Ubuntu 22.04 and RHEL 9. **0.1.11 lowered the floor to glibc 2.34**: Ubuntu
22.04+, Debian 12+ and RHEL/Rocky/Alma 9+ now work. Ubuntu 20.04 and Debian 11 (glibc 2.31) remain
too old.

There is no static build to fall back on: static linking breaks NVML's runtime `dlopen`, and you
would lose GPU metrics. Build from source on such a system if you need one.

Since 0.1.11 the npm launcher also probes the binary with `--help` after downloading and reports the
loader's own message, so a glibc mismatch surfaces as
`the installed binary does not run on this machine: ...` rather than a bare spawn error. On an older
package the same problem appears as the binary exiting immediately with that loader message.

### Windows: `lomora` holds the terminal open and closing it stops the server

That is pre-0.1.11 behaviour. Since 0.1.11 a launch from a shell hands the work to a console-less
background process: the prompt returns immediately, the server keeps running behind the tray icon,
and closing the terminal no longer takes it down. `lomora --console` is the foreground mode — it
keeps the log and Ctrl+C, which is what you want while reading startup messages. `--hidden` is now
only the explicit spelling of the default for a shell launch.

On Linux and macOS there is no console window to close, so `--hidden` is the detached path there: it
re-executes in a new session, waits until the dashboard answers, prints the address and pid, and
exits. The pid file is `data/lomora.pid` beside the database, so `kill "$(cat data/lomora.pid)"`
stops it.

### Windows: "Windows protected your PC"

The binaries are unsigned. A file downloaded through a browser carries a Mark-of-the-Web and gets
the SmartScreen prompt once (*More info* → *Run anyway*). `curl`, `Invoke-WebRequest` and npm do not
set that mark, so terminal installs normally start without the dialog. `Get-AuthenticodeSignature`
reports `NotSigned` either way, and managed devices (AppLocker, WDAC, S mode) can still refuse the
binary regardless of how it arrived. See *About code signing* in the README.

### macOS: "cannot be opened because the developer cannot be verified"

Gatekeeper quarantine, on a copy that came through a browser. Right-click → *Open*, or:

```bash
xattr -d com.apple.quarantine /path/to/lomora
```

curl and npm downloads are usually not quarantined, so they start without this.

### Dashboard unreachable at http://localhost:8501

Check the process is alive and listening, and whether something else holds the port:

```bash
ss -ltnp | grep 8501      # Linux
lsof -iTCP:8501 -sTCP:LISTEN
```

Set `PORT=9000` (or any free port) to move it. A crash can leave a stale `data/lomora.pid`; it is
only a convenience for `kill`, so remove it if the process is gone.

### WSL: `lomora` started but no browser appeared

That is the design, not a crash. The tray icon, its balloon and the "Open dashboard" menu are
Windows-only; on Linux `lomora` starts an HTTP server in the foreground, prints
`Lomora listening on http://0.0.0.0:8501`, and holds that terminal. Open the address yourself.
Closing the WSL window or pressing Ctrl+C stops the server — use `lomora --hidden` for a detached
one, which writes `data/lomora.pid`.

Split the problem in two from inside WSL, with the server still running in another terminal:

```bash
curl -sS http://127.0.0.1:8501/api/version
```

A JSON reply means the server is healthy and only the Windows -> WSL2 hop is failing. First try
`http://127.0.0.1:8501` rather than `localhost` (Windows often resolves `localhost` to `::1`
first, while the server binds IPv4 only). If that also fails, the relay is down — see below.

If **both** `localhost:8501` and `127.0.0.1:8501` fail from the Windows browser while the WSL-side
curl succeeds, the WSL2 loopback relay is not listening at all — the `::1` advice above is not the
cause in that case. Check it from PowerShell on the Windows host (a proxy-free request; an HTTP
proxy in the environment can otherwise answer with a misleading 502):

```powershell
curl.exe --noproxy '*' -sS http://127.0.0.1:8501/api/version
netstat -an | findstr :8501
```

`Connection refused` plus no listener means the relay is down. The dashboard is still reachable on
the WSL VM's own address, because Lomora binds `0.0.0.0`:

```bash
hostname -I | awk '{print $1}'     # inside WSL, e.g. 172.23.110.108
```

Open `http://<that-ip>:8501` in the Windows browser. That address changes when WSL restarts, so
read it again after a reboot. To restore loopback instead, run `wsl --shutdown` in PowerShell and
reopen WSL; that also stops a detached `--hidden` server, so start it again afterwards. Adding an
explicit `localhostForwarding=true` to `.wslconfig` first makes the intent clear:

```ini
[wsl2]
localhostForwarding=true
```

If the WSL-side curl above is refused, nothing is listening; read the terminal you launched it in:

- `GLIBC_2.34 not found` — the release binary needs glibc 2.34+ (`ldd --version`), i.e. Ubuntu
  22.04+ or Debian 12+. An older WSL distro cannot run it; build from source there.
- A SQLite error — `data/lomora.db` is created beside the working directory. Run from a native
  Linux directory such as `~/lomora`, not `/mnt/c/...`, where DrvFS locking is unreliable.
- `Address already in use` — set `PORT=9000`.
- `command -v lomora` — WSL appends the Windows PATH, so make sure it resolves to the Linux
  binary and not a Windows `lomora.exe`.

### GPU counters are missing

They need an NVIDIA driver exposing NVML (`nvml.dll` on Windows, `libnvidia-ml.so` on Linux). Without
one, CPU and RAM still report and the dashboard explains why: `GET /api/hardware` returns a
`gpu_status` of `driver_missing`, `no_nvidia`, `unsupported` (macOS) or `unknown`. NVML is
re-detected every ten seconds, so installing the driver and reloading the page is enough — no
restart. The CUDA toolkit is not required; NVML ships with the display driver.

### `lomora --hidne` started a server instead of complaining

It should not: unknown options print the full usage and exit with status 2, and `--help` prints the
same usage with status 0. If an unknown option was accepted, that build predates option parsing —
upgrade.

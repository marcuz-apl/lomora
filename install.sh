#!/bin/sh
# Lomora installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.sh | sh
#
# Because `curl | sh` cannot pass arguments, everything is configured through the
# environment (set them inline, e.g. `LOMORA_VERSION=v0.1.11 curl ... | sh`):
#
#   LOMORA_VERSION=v0.1.11     Pin a release tag. Default: the latest release.
#   LOMORA_INSTALL_DIR=DIR    Install location. Default: $HOME/.local/bin.
#   LOMORA_SKIP_VERIFY=1      Skip the SHA-256 check (not recommended).
#   LOMORA_RELEASE_BASE=URL   Fetch assets from a mirror of the release instead.
#
# The script never needs sudo: it installs into a user-writable directory and
# tells you how to add that directory to PATH when it is missing.
set -eu

REPO='marcuz-apl/lomora'

fail() { printf 'lomora: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail 'curl is required'
command -v uname >/dev/null 2>&1 || fail 'uname is required'

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux) platform='linux' ;;
    Darwin) platform='macos' ;;
    MINGW* | MSYS* | CYGWIN* | Windows_NT)
        fail 'on Windows use PowerShell: irm https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.ps1 | iex' ;;
    *) fail "unsupported operating system: $os" ;;
esac

case "$platform:$arch" in
    linux:x86_64 | linux:amd64)   asset='lomora-linux-x86_64' ;;
    macos:arm64 | macos:aarch64)  asset='lomora-macos-aarch64' ;;
    macos:x86_64)
        fail 'no Intel Mac binary is published yet; build from source with: cargo build --release' ;;
    linux:aarch64 | linux:arm64)
        fail 'no Linux ARM64 binary is published yet; build from source with: cargo build --release' ;;
    *) fail "unsupported platform: $os/$arch" ;;
esac

# Resolving /releases/latest through its redirect avoids the GitHub API's
# unauthenticated rate limit; the API is only a fallback.
if [ -n "${LOMORA_VERSION:-}" ]; then
    case "$LOMORA_VERSION" in
        v*) tag="$LOMORA_VERSION" ;;
        *)  tag="v$LOMORA_VERSION" ;;
    esac
else
    tag="$(curl -fsSL -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest" 2>/dev/null \
        | sed -n 's#.*/tag/\([^/]*\)$#\1#p')" || tag=''
    if [ -z "$tag" ]; then
        tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | head -n 1)" || tag=''
    fi
fi
[ -n "$tag" ] || fail 'could not determine a release tag; set LOMORA_VERSION=v0.1.11 and retry'

base="${LOMORA_RELEASE_BASE:-https://github.com/$REPO/releases/download/$tag}"
base="${base%/}"
install_dir="${LOMORA_INSTALL_DIR:-$HOME/.local/bin}"

tmp="$(mktemp -d)" || fail 'could not create a temporary directory'
staged="$install_dir/.lomora.new.$$"
trap 'rm -rf "$tmp"; rm -f "$staged"' EXIT HUP INT TERM

printf 'Downloading %s (%s)...\n' "$asset" "$tag"
curl -fsSL --retry 3 --retry-delay 1 -o "$tmp/$asset" "$base/$asset" \
    || fail "download failed: $base/$asset"
# Releases older than the checksum job have no SHA256SUMS; verification then
# degrades to HTTPS only instead of failing the install.
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null \
    || rm -f "$tmp/SHA256SUMS"

if [ "${LOMORA_SKIP_VERIFY:-0}" = 1 ]; then
    printf '  checksum verification skipped (LOMORA_SKIP_VERIFY=1)\n'
elif [ ! -f "$tmp/SHA256SUMS" ]; then
    printf '  note: no SHA256SUMS in this release; verified by HTTPS only\n' >&2
else
    if command -v sha256sum >/dev/null 2>&1; then
        hasher='sha256sum'
    elif command -v shasum >/dev/null 2>&1; then
        hasher='shasum -a 256'
    else
        hasher=''
    fi
    if [ -z "$hasher" ]; then
        printf '  note: neither sha256sum nor shasum is available; skipping verification\n' >&2
    else
        expected="$(awk -v a="$asset" '{ n=$2; sub(/^\*/, "", n); if (n == a) { print $1; exit } }' \
            "$tmp/SHA256SUMS")"
        [ -n "$expected" ] || fail "$asset is not listed in SHA256SUMS"
        actual="$($hasher "$tmp/$asset" | awk '{print $1}')"
        [ "$actual" = "$expected" ] \
            || fail "checksum mismatch for $asset (expected $expected, got $actual)"
        printf '  checksum OK\n'
    fi
fi

mkdir -p "$install_dir" || fail "cannot create $install_dir"
cp "$tmp/$asset" "$staged" || fail "cannot write to $install_dir"
chmod 755 "$staged"
mv -f "$staged" "$install_dir/lomora" || fail "cannot install into $install_dir"

case ":$PATH:" in
    *":$install_dir:"*) on_path=1 ;;
    *) on_path=0 ;;
esac

printf '\nInstalled lomora %s -> %s/lomora\n' "$tag" "$install_dir"
if [ "$on_path" = 0 ]; then
    printf 'Add it to PATH (and to your shell profile):\n    export PATH="%s:$PATH"\n' "$install_dir"
fi
printf '\nRun it from a folder of its own - the database is created at ./data/lomora.db:\n'
printf '    mkdir -p ~/lomora && cd ~/lomora\n'
printf '    lomora             # then open http://localhost:8501\n'
printf '\nOther options: lomora --help, PORT=9000 lomora, LOMORA_DB=/path/to/lomora.db lomora\n'
if [ "$platform" = macos ]; then
    printf 'This download is not quarantined; a browser-downloaded copy may need right-click -> Open.\n'
fi

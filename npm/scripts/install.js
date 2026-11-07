'use strict';

// Fetches the release binary that matches this platform.
//
// npm 12 blocks lifecycle scripts by default ("install scripts blocked because
// they are not covered by allowScripts"), which silently skipped the old
// postinstall hook and left the launcher without a binary. So this is no longer
// wired up as postinstall: bin/lomora.js runs it on first use instead, and it
// can still be run by hand (`node scripts/install.js`).
//
// The binary goes beside the launcher when that directory is writable, and into
// a per-user cache otherwise (a global install owned by root, for example).
//
// Node builtins only: the npm package has no runtime dependencies, and the
// binary itself needs no Node runtime once it has been installed.
//
// Environment variables:
//   LOMORA_BINARY=/path/to/lomora   Copy a local binary instead of downloading.
//   LOMORA_RELEASE_BASE=URL         Use a mirror (or a local server for tests).
//   LOMORA_CACHE_DIR=DIR            Where to put the binary if ../bin is read-only.
//   LOMORA_SKIP_DOWNLOAD=1          Leave the download to another mechanism.
//   LOMORA_SKIP_VERIFY=1            Skip the SHA-256 check.
//   LOMORA_VERSION=0.1.8            Override the release tag (defaults to package version).

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const http = require('node:http');
const https = require('node:https');

const REPO = 'marcuz-apl/lomora';
const binDir = path.join(__dirname, '..', 'bin');
const exe = process.platform === 'win32' ? 'lomora.exe' : 'lomora';
const pkgVersion = require('../package.json').version;
const version = process.env.LOMORA_VERSION || pkgVersion;
const tag = version.startsWith('v') ? version : `v${version}`;
const base = (process.env.LOMORA_RELEASE_BASE ||
  `https://github.com/${REPO}/releases/download/${tag}`).replace(/\/$/, '');

const ASSETS = {
  'linux x64': 'lomora-linux-x86_64',
  'darwin arm64': 'lomora-macos-aarch64',
  'win32 x64': 'lomora-windows-x86_64.exe',
};

const UNSUPPORTED = {
  'darwin x64': 'no Intel Mac binary is published yet',
  'linux arm64': 'no Linux ARM64 binary is published yet',
  'win32 arm64': 'no Windows ARM64 binary is published yet',
};

function cacheDir() {
  if (process.env.LOMORA_CACHE_DIR) return path.resolve(process.env.LOMORA_CACHE_DIR);
  if (process.platform === 'win32') {
    return path.join(process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local'), 'lomora');
  }
  return path.join(process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache'), 'lomora');
}

function writable(dir) {
  try {
    fs.mkdirSync(dir, { recursive: true });
    fs.accessSync(dir, fs.constants.W_OK);
    return true;
  } catch {
    return false;
  }
}

// Where the launcher looks for the binary and the installer puts it: an existing
// copy wins, then the package's own bin directory, then the user cache.
function targetPath() {
  const beside = path.join(binDir, exe);
  if (fs.existsSync(beside)) return beside;
  const cached = path.join(cacheDir(), exe);
  if (fs.existsSync(cached)) return cached;
  return writable(binDir) ? beside : cached;
}

function request(url, redirectsLeft = 6) {
  return new Promise((resolve, reject) => {
    const lib = url.startsWith('http:') ? http : https;
    const req = lib.get(url, { headers: { 'User-Agent': 'lomora-npm-installer' } }, (res) => {
      const status = res.statusCode || 0;
      if ([301, 302, 303, 307, 308].includes(status) && res.headers.location) {
        res.resume();
        if (redirectsLeft === 0) return reject(new Error('too many redirects'));
        const next = new URL(res.headers.location, url).toString();
        return resolve(request(next, redirectsLeft - 1));
      }
      if (status !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${status} for ${url}`));
      }
      resolve(res);
    });
    req.setTimeout(30000, () => req.destroy(new Error(`timed out fetching ${url}`)));
    req.on('error', reject);
  });
}

async function downloadToFile(url, dest) {
  const res = await request(url);
  await new Promise((resolve, reject) => {
    const out = fs.createWriteStream(dest);
    res.on('error', reject);
    out.on('error', reject);
    out.on('finish', resolve);
    res.pipe(out);
  });
}

async function downloadText(url) {
  const res = await request(url);
  let body = '';
  for await (const chunk of res) body += chunk.toString('utf8');
  return body;
}

function expectedHash(sums, asset) {
  for (const raw of sums.split(/\r?\n/)) {
    const match = raw.match(/^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$/);
    if (match && path.basename(match[2]) === asset) return match[1].toLowerCase();
  }
  return null;
}

// Resolves to the installed binary path, or null when the download was skipped.
async function ensureBinary() {
  if (process.env.LOMORA_SKIP_DOWNLOAD === '1') {
    console.log('lomora: download skipped (LOMORA_SKIP_DOWNLOAD=1)');
    return null;
  }

  const platform = `${process.platform} ${process.arch}`;
  const asset = ASSETS[platform];
  if (!asset) {
    const why = UNSUPPORTED[platform] || `platform ${platform} has no published binary`;
    throw new Error(`${why}. Build from source instead: cargo build --release ` +
      `(see https://github.com/${REPO}#build-from-source)`);
  }

  const dest = targetPath();
  const destDir = path.dirname(dest);
  fs.mkdirSync(destDir, { recursive: true });

  if (process.env.LOMORA_BINARY) {
    const local = path.resolve(process.env.LOMORA_BINARY);
    fs.copyFileSync(local, dest);
    if (process.platform !== 'win32') fs.chmodSync(dest, 0o755);
    console.log(`lomora: installed local binary ${local} -> ${dest}`);
    return dest;
  }

  const tmp = path.join(destDir, `.${asset}.${process.pid}.download`);
  try {
    console.log(`lomora: downloading ${asset} (${tag})...`);
    await downloadToFile(`${base}/${asset}`, tmp);

    if (process.env.LOMORA_SKIP_VERIFY === '1') {
      console.log('lomora: checksum verification skipped (LOMORA_SKIP_VERIFY=1)');
    } else {
      let sums;
      try {
        sums = await downloadText(`${base}/SHA256SUMS`);
      } catch {
        // Releases published before the checksum job have no SHA256SUMS.
        console.warn('lomora: no SHA256SUMS in this release; verified by HTTPS only');
      }
      if (sums) {
        const expected = expectedHash(sums, asset);
        if (!expected) throw new Error(`${asset} is not listed in SHA256SUMS`);
        const actual = crypto.createHash('sha256').update(fs.readFileSync(tmp)).digest('hex');
        if (actual !== expected) {
          throw new Error(`checksum mismatch for ${asset} (expected ${expected}, got ${actual})`);
        }
        console.log('lomora: checksum OK');
      }
    }

    fs.rmSync(dest, { force: true });
    fs.renameSync(tmp, dest);
    if (process.platform !== 'win32') fs.chmodSync(dest, 0o755);
    console.log(`lomora: installed ${dest}`);
    return dest;
  } finally {
    fs.rmSync(tmp, { force: true });
  }
}

module.exports = { ensureBinary, targetPath };

if (require.main === module) {
  ensureBinary()
    .then((dest) => {
      if (dest) console.log('lomora: run `lomora` from a folder of its own, then open http://localhost:8501');
    })
    .catch((err) => {
      console.error(`lomora: installation failed: ${err.message}`);
      console.error(`lomora: download a binary manually from https://github.com/${REPO}/releases/latest`);
      console.error('lomora: or set LOMORA_BINARY=/path/to/lomora and run `node scripts/install.js` again');
      process.exit(1);
    });
}

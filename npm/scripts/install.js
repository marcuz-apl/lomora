'use strict';

// Downloads the release binary that matches this platform into ../bin.
// Node builtins only: the npm package has no runtime dependencies, and the
// binary itself needs no Node runtime once it has been installed.
//
// Environment variables:
//   LOMORA_BINARY=/path/to/lomora   Copy a local binary instead of downloading.
//   LOMORA_RELEASE_BASE=URL         Use a mirror (or a local server for tests).
//   LOMORA_SKIP_DOWNLOAD=1          Leave the download to another mechanism.
//   LOMORA_SKIP_VERIFY=1            Skip the SHA-256 check.
//   LOMORA_VERSION=0.1.8            Override the release tag (defaults to package version).

const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const http = require('node:http');
const https = require('node:https');

const REPO = 'marcuz-apl/lomora';
const binDir = path.join(__dirname, '..', 'bin');
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

async function main() {
  if (process.env.LOMORA_SKIP_DOWNLOAD === '1') {
    console.log('lomora: download skipped (LOMORA_SKIP_DOWNLOAD=1)');
    return;
  }

  const platform = `${process.platform} ${process.arch}`;
  const asset = ASSETS[platform];
  if (!asset) {
    const why = UNSUPPORTED[platform] || `platform ${platform} has no published binary`;
    throw new Error(`${why}. Build from source instead: cargo build --release ` +
      `(see https://github.com/${REPO}#build-from-source)`);
  }

  const exe = process.platform === 'win32' ? 'lomora.exe' : 'lomora';
  const dest = path.join(binDir, exe);
  fs.mkdirSync(binDir, { recursive: true });

  if (process.env.LOMORA_BINARY) {
    const local = path.resolve(process.env.LOMORA_BINARY);
    fs.copyFileSync(local, dest);
    if (process.platform !== 'win32') fs.chmodSync(dest, 0o755);
    console.log(`lomora: installed local binary ${local} -> ${dest}`);
    return;
  }

  const tmp = path.join(binDir, `.${asset}.download`);
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

    fs.copyFileSync(tmp, dest);
    if (process.platform !== 'win32') fs.chmodSync(dest, 0o755);
    console.log(`lomora: installed ${dest}`);
    console.log('lomora: run `lomora` from a folder of its own, then open http://localhost:8501');
  } finally {
    fs.rmSync(tmp, { force: true });
  }
}

main().catch((err) => {
  console.error(`lomora: installation failed: ${err.message}`);
  console.error(`lomora: download a binary manually from https://github.com/${REPO}/releases/latest`);
  console.error('lomora: or set LOMORA_BINARY=/path/to/lomora and run `npm rebuild lomora`');
  process.exit(1);
});

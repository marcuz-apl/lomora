#!/usr/bin/env node
'use strict';

// Thin launcher: npm installs this file as the `lomora` command, and the real
// work belongs to the Rust binary.
//
// The binary is fetched on first use rather than from a postinstall hook,
// because npm 12 blocks lifecycle scripts by default. That keeps
// `npm install -g lomora` working with no allowScripts approval.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const { targetPath } = require('../scripts/install.js');

const exe = process.platform === 'win32' ? 'lomora.exe' : 'lomora';
let binary = targetPath();

if (!fs.existsSync(binary)) {
  console.error('lomora: first run, fetching the release binary (this happens once)...');
  const installer = path.join(__dirname, '..', 'scripts', 'install.js');
  const result = spawnSync(process.execPath, [installer], { stdio: 'inherit' });
  binary = targetPath();

  if (result.status !== 0 || !fs.existsSync(binary)) {
    console.error(`lomora: could not obtain the ${exe} binary.`);
    console.error('lomora: check your network, or download it by hand from');
    console.error('lomora:   https://github.com/marcuz-apl/lomora/releases/latest');
    console.error('lomora: and point LOMORA_BINARY at the file, then run lomora again.');
    process.exit(1);
  }
}

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });
const SIGNALS = ['SIGINT', 'SIGTERM', 'SIGHUP'];

for (const signal of SIGNALS) {
  process.on(signal, () => child.kill(signal));
}

child.on('error', (err) => {
  console.error(`lomora: could not start ${binary}: ${err.message}`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (!signal) {
    process.exit(code === null ? 1 : code);
    return;
  }
  // Drop our forwarding handlers before re-raising, otherwise the handler
  // would catch the signal again and only try to kill an already-dead child,
  // leaving `npm`-launched Lomora hanging after Ctrl+C.
  for (const name of SIGNALS) process.removeAllListeners(name);
  process.kill(process.pid, signal);
  // Some hosts ignore re-raised signals; make sure the shim still terminates.
  setTimeout(() => process.exit(128 + (os.constants.signals[signal] || 2)), 100).unref();
});

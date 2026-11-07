#!/usr/bin/env node
'use strict';

// Thin shim: npm installs this file as the `lomora` command, and the real work
// belongs to the Rust binary that scripts/install.js placed next to it.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const exe = process.platform === 'win32' ? 'lomora.exe' : 'lomora';
const binary = path.join(__dirname, exe);

if (!fs.existsSync(binary)) {
  console.error(`lomora: the ${exe} binary is missing from ${__dirname}.`);
  console.error('Reinstall it with:  npm rebuild lomora');
  console.error('Or install without npm:  curl -fsSL https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.sh | sh');
  process.exit(1);
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

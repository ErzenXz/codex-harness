#!/usr/bin/env node
'use strict';

const { version } = require('../package.json');
const platform = `${process.platform}-${process.arch}`;
const supported = new Set(['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-arm64', 'win32-x64']);

function fail(message) {
  console.error(`codex-harness: ${message}`);
  process.exit(1);
}

if (!supported.has(platform)) fail(`Unsupported platform ${platform}. Build from source: https://github.com/ErzenXz/codex-harness`);
const name = `@erzenx/codex-harness-${platform}`;
let binary;
try {
  const manifest = require.resolve(`${name}/package.json`);
  if (require(manifest).version !== version) fail(`Native package version mismatch. Reinstall codex-harness@${version}.`);
  binary = require('node:path').join(require('node:path').dirname(manifest), 'bin', `codex-harness${process.platform === 'win32' ? '.exe' : ''}`);
  require('node:fs').accessSync(binary, require('node:fs').constants.X_OK);
} catch {
  fail(`Missing ${name}@${version}. Reinstall with optional dependencies enabled (no --no-optional/--omit=optional). No binary is downloaded at runtime.`);
}

const args = process.argv.slice(2);
if (args.length === 1 && args[0] === '--print-binary-path') {
  console.log(binary);
} else if (process.platform !== 'win32' && typeof process.execve === 'function') {
  // Replace Node entirely: no resident launcher, pipe proxy, signal proxy or extra RSS.
  process.execve(binary, [binary, ...args], process.env);
} else {
  const { spawn } = require('node:child_process');
  const child = spawn(binary, args, { stdio: 'inherit', windowsHide: true });
  // Windows consoles already deliver Ctrl+C to the native child. Sending
  // child.kill('SIGINT') there would forcibly terminate it before cleanup.
  if (process.platform === 'win32') process.on('SIGINT', () => {});
  const signals = process.platform === 'win32' ? ['SIGTERM'] : ['SIGINT', 'SIGTERM', 'SIGHUP'];
  for (const signal of signals) process.on(signal, () => child.kill(signal));
  child.on('error', (error) => fail(`Cannot launch native binary: ${error.message}`));
  child.on('exit', (code, signal) => {
    if (signal && process.platform !== 'win32') {
      process.removeAllListeners(signal);
      process.kill(process.pid, signal);
    } else {
      process.exitCode = code ?? 1;
    }
  });
}

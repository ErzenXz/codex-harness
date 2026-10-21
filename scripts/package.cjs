#!/usr/bin/env node
'use strict';
// Stages an allowlisted package outside the checkout. No dependencies or lifecycle scripts.
const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname, '..');
const cli = require('../npm/cli/package.json');
const platforms = Object.keys(cli.optionalDependencies).map((name) => name.replace('@erzenx/codex-harness-', ''));
function stage({ out, platform, binary }) {
  if (!out || !path.isAbsolute(out)) throw new Error('--out must be an absolute scratch directory');
  out = path.resolve(out);
  if (out === root || out.startsWith(root + path.sep)) throw new Error('Stage packages outside the checkout');
  if (fs.existsSync(out)) throw new Error(`Refusing to overwrite ${out}`);
  if (require('../package.json').version !== cli.version || !fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8').includes(`version = "${cli.version}"`)) throw new Error('Rust/npm version mismatch');
  let manifest;
  if (platform === 'cli') {
    manifest = cli;
  } else {
    if (!platforms.includes(platform)) throw new Error('Unknown platform');
    if (!binary || !fs.statSync(binary).isFile()) throw new Error('Missing native binary');
    const [os, cpu] = platform.split('-');
    manifest = {
      name: `@erzenx/codex-harness-${platform}`, version: cli.version,
      description: `Native codex-harness binary for ${platform}`, license: cli.license,
      repository: cli.repository, os: [os], cpu: [cpu],
      files: ['bin', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'README.md'],
      publishConfig: cli.publishConfig,
    };
  }
  fs.mkdirSync(path.join(out, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(out, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
  for (const name of ['LICENSE', 'THIRD_PARTY_NOTICES.md']) fs.copyFileSync(path.join(root, name), path.join(out, name));
  if (platform === 'cli') {
    fs.copyFileSync(path.join(root, 'npm/cli/bin/codex-harness.cjs'), path.join(out, 'bin/codex-harness.cjs'));
    fs.chmodSync(path.join(out, 'bin/codex-harness.cjs'), 0o755);
    fs.copyFileSync(path.join(root, 'README.md'), path.join(out, 'README.md'));
  } else {
    const target = path.join(out, 'bin', `codex-harness${platform.startsWith('win32') ? '.exe' : ''}`);
    fs.copyFileSync(binary, target);
    fs.chmodSync(target, 0o755);
    fs.writeFileSync(path.join(out, 'README.md'), `# ${manifest.name}\n\nPlatform binary for [codex-harness](https://www.npmjs.com/package/codex-harness). Install the main package instead. Linux binaries use static musl; no glibc dependency.\n`);
  }
  return manifest;
}
module.exports = { stage, platforms };
if (require.main === module) {
  const args = process.argv.slice(2), options = {};
  while (args.length) {
    const key = args.shift();
    if (!['--out', '--platform', '--binary'].includes(key) || !args.length) throw new Error('Use --out DIR --platform PLATFORM [--binary FILE]');
    options[key.slice(2)] = args.shift();
  }
  console.log(JSON.stringify(stage(options)));
}

'use strict';
// Install the actual packed launcher with only our reviewed local native tarball.
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const { stage } = require('./package.cjs');
const cli = require('../npm/cli/package.json');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'harness-install-'));
function pnpm(args, cwd) {
  // Actions invokes this script through pnpm; run that same JS CLI on Windows
  // rather than passing paths through cmd.exe's shell interpolation.
  const script = process.env.npm_execpath;
  const js = script && /\.[cm]?js$/.test(script);
  const command = js ? process.execPath : (script || 'pnpm');
  const result = spawnSync(command, js ? [script, ...args] : args, { cwd, stdio: 'inherit' });
  if (result.status !== 0) throw new Error(`pnpm ${args[0]} failed (${result.status})`);
}
try {
  const platform = `${process.platform}-${process.arch}`;
  const binary = path.resolve(process.env.HARNESS_BIN || `target/debug/codex-harness${process.platform === 'win32' ? '.exe' : ''}`);
  const cliDir = path.join(root, 'cli'), nativeDir = path.join(root, 'native');
  stage({ out: cliDir, platform: 'cli' });
  stage({ out: nativeDir, platform, binary });
  const cliTar = path.join(root, 'cli.tgz'), nativeTar = path.join(root, 'native.tgz');
  pnpm(['pack', '--out', cliTar], cliDir);
  pnpm(['pack', '--out', nativeTar], nativeDir);
  const app = path.join(root, 'app'); fs.mkdirSync(app);
  fs.writeFileSync(path.join(app, 'package.json'), JSON.stringify({ private: true, dependencies: { 'codex-harness': `file:${cliTar}` } }));
  const overrides = Object.keys(cli.optionalDependencies).map((name) => `  ${JSON.stringify(name)}: ${JSON.stringify(name.endsWith(`-${platform}`) ? `file:${nativeTar}` : '-')}`).join('\n');
  fs.writeFileSync(path.join(app, 'pnpm-workspace.yaml'), `packages: []\nminimumReleaseAge: 1440\nstrictDepBuilds: true\nblockExoticSubdeps: true\nallowBuilds: {}\noverrides:\n${overrides}\n`);
  pnpm(['install', '--offline', '--ignore-scripts'], app);
  const launcher = path.join(app, 'node_modules/codex-harness/bin/codex-harness.cjs');
  const result = spawnSync(process.execPath, [launcher, '--version'], { encoding: 'utf8' });
  if (result.status !== 0 || result.stderr || result.stdout.trim() !== `codex-harness ${cli.version}`) throw new Error(`Packed installation failed: ${result.stderr}`);
  console.log('Packed offline installation passed with scripts disabled.');
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

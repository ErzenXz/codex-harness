'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync, spawn } = require('node:child_process');
const { stage, platforms } = require('../scripts/package.cjs');
const cliManifest = require('../npm/cli/package.json');

function fixture(t, native = false) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'piko package 世界 '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const cli = path.join(root, 'cli');
  stage({ out: cli, platform: 'cli' });
  const platform = `${process.platform}-${process.arch}`;
  const binary = native ? path.resolve(process.env.PIKO_BIN || process.env.HARNESS_BIN || `target/debug/piko${process.platform === 'win32' ? '.exe' : ''}`) : path.join(root, 'fixture');
  if (!native) fs.writeFileSync(binary, 'fixture');
  const pkg = path.join(cli, 'node_modules', '@erzenx', `piko-${platform}`);
  stage({ out: pkg, platform, binary });
  return { root, cli, pkg, launcher: path.join(cli, 'bin', 'piko.cjs') };
}

test('all six platform manifests are script-free and exactly pinned', (t) => {
  const { root } = fixture(t);
  const binary = path.join(root, 'fixture');
  for (const platform of platforms) {
    const out = path.join(root, platform);
    const manifest = stage({ out, platform, binary });
    assert.equal(cliManifest.optionalDependencies[manifest.name], manifest.version);
    assert.equal(manifest.scripts, undefined);
    assert.equal(manifest.dependencies, undefined);
    assert.equal(manifest.os[0], platform.split('-')[0]);
    assert.equal(manifest.cpu[0], platform.split('-')[1]);
    assert.deepEqual(fs.readdirSync(out).sort(), ['LICENSE', 'README.md', 'THIRD_PARTY_NOTICES.md', 'bin', 'package.json']);
  }
  assert.equal(cliManifest.scripts, undefined);
  assert.equal(cliManifest.name, '@erzenx/piko');
  assert.deepEqual(cliManifest.bin, { piko: 'bin/piko.cjs', 'codex-harness': 'bin/piko.cjs' });
  assert.deepEqual(fs.readdirSync(path.join(root, 'cli', 'bin')), ['piko.cjs']);
});

test('staging refuses source-tree output and overwrites', (t) => {
  const { cli } = fixture(t);
  assert.throws(() => stage({ out: cli, platform: 'cli' }), /overwrite/);
  assert.throws(() => stage({ out: path.resolve('npm/stage'), platform: 'cli' }), /outside/);
});

test('missing native package and version skew fail clearly on stderr', (t) => {
  const { launcher, pkg } = fixture(t);
  const manifest = path.join(pkg, 'package.json');
  const data = JSON.parse(fs.readFileSync(manifest));
  data.version = '0.0.0';
  fs.writeFileSync(manifest, JSON.stringify(data));
  let result = spawnSync(process.execPath, [launcher, '--version'], { encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /version mismatch/);
  fs.rmSync(pkg, { recursive: true });
  result = spawnSync(process.execPath, [launcher], { encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /optional dependencies/);
});

test('real launcher preserves version, exit code, Unicode paths and quiet ACP stdio', async (t) => {
  const { launcher, root, pkg } = fixture(t, true);
  const launch = (args, extra = {}) => spawnSync(process.execPath, [launcher, ...args], { encoding: 'utf8', ...extra });
  let result = launch(['--version']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stderr, '');
  assert.equal(result.stdout.trim(), `piko ${cliManifest.version}`);
  result = launch(['--print-binary-path']);
  assert.equal(result.status, 0);
  assert.equal(fs.realpathSync(result.stdout.trim()), fs.realpathSync(path.join(pkg, 'bin', `piko${process.platform === 'win32' ? '.exe' : ''}`)));
  const config = path.join(root, 'config 世界.toml');
  fs.writeFileSync(config, `auth = "none"\nstate_dir = ${JSON.stringify(path.join(root, 'state'))}\n`);
  const input = JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: 1 } }) + '\n';
  result = launch(['--config', config], { input });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stderr, '');
  assert.equal(JSON.parse(result.stdout).result.protocolVersion, 1);
  result = launch(['--invalid']);
  assert.equal(result.status, 2);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /Invalid arguments/);
  if (process.platform !== 'win32') {
    const child = spawn(process.execPath, [launcher, '--config', config], { stdio: ['pipe', 'pipe', 'pipe'] });
    child.stdin.write(input);
    await new Promise((resolve, reject) => { child.stdout.once('data', resolve); child.once('error', reject); });
    child.kill('SIGTERM');
    const code = await new Promise((resolve) => child.once('exit', resolve));
    assert.equal(code, 0);
  }
});

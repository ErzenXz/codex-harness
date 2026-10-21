'use strict';
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const { stage, platforms } = require('./package.cjs');
const cli = require('../npm/cli/package.json');
function run(command, args) {
  const result = spawnSync(command, args, { stdio: 'inherit' });
  if (result.status !== 0) throw new Error(`${command} failed (${result.status})`);
}
const [action, dir] = process.argv.slice(2);
if (!dir || !path.isAbsolute(dir) || !['validate', 'publish', 'github'].includes(action)) throw new Error('Use validate|publish|github ABSOLUTE_ARTIFACT_DIR');
const tag = `v${cli.version}`;
if ((process.env.RELEASE_REF || process.env.GITHUB_REF_NAME) !== tag) throw new Error(`Release must run on tag ${tag}`);
async function published(name) {
  const response = await fetch(`https://registry.npmjs.org/${encodeURIComponent(name)}/${cli.version}`, { signal: AbortSignal.timeout(15000) });
  if (response.status === 404) return false;
  if (!response.ok) throw new Error(`Registry check failed: ${response.status}`);
  const manifest = await response.json();
  if (manifest.version !== cli.version) throw new Error('Registry version mismatch');
  return true;
}
async function main() {
  for (const platform of platforms) run('python3', ['scripts/verify-package.py', path.join(dir, `native-${platform}.tgz`), platform]);
  if (action === 'validate') return;
  if (action === 'publish') {
    for (const platform of platforms) {
      const name = `@erzenx/codex-harness-${platform}`;
      if (!(await published(name))) run('pnpm', ['publish', path.join(dir, `native-${platform}.tgz`), '--access', 'public', '--provenance', '--ignore-scripts', '--no-git-checks']);
      if (!(await published(name))) throw new Error(`Native package unavailable: ${name}`);
    }
    const out = path.join(process.env.RUNNER_TEMP, 'cli-package');
    stage({ out, platform: 'cli' });
    const tarball = path.join(dir, `codex-harness-${cli.version}.tgz`);
    run('pnpm', ['--dir', out, 'pack', '--out', tarball]);
    run('python3', ['scripts/verify-package.py', tarball, 'cli']);
    if (!(await published(cli.name))) run('pnpm', ['publish', tarball, '--access', 'public', '--provenance', '--ignore-scripts', '--no-git-checks']);
    if (!(await published(cli.name))) throw new Error('Launcher package unavailable after publish');
  } else {
    const files = fs.readdirSync(dir).filter((name) => /\.(tgz|tar\.gz|zip)$/.test(name)).sort();
    if (files.length !== 13) throw new Error('Expected six npm native packages, six archives and one npm launcher');
    const sums = files.map((name) => `${crypto.createHash('sha256').update(fs.readFileSync(path.join(dir, name))).digest('hex')}  ${name}\n`).join('');
    fs.writeFileSync(path.join(dir, 'SHA256SUMS'), sums);
    run('gh', ['release', 'create', tag, ...files.map((name) => path.join(dir, name)), path.join(dir, 'SHA256SUMS'), '--verify-tag', '--title', `Codex Harness ${cli.version}`, '--notes-file', 'CHANGELOG.md']);
  }
}
main().catch((error) => { console.error(error.message); process.exitCode = 1; });

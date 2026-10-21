'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
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
const nativeFile = (platform) => path.join(dir, `native-${platform}.tgz`);
const archiveFile = (platform) => path.join(dir, `codex-harness-${cli.version}-${platform}.${platform.startsWith('win32') ? 'zip' : 'tar.gz'}`);
function preflight() {
  const expected = platforms.flatMap((platform) => [nativeFile(platform), archiveFile(platform)]).map((file) => path.basename(file));
  const launcher = `codex-harness-${cli.version}.tgz`;
  const actual = fs.readdirSync(dir).filter((name) => /\.(tgz|tar\.gz|zip)$/.test(name) && name !== launcher);
  if (actual.length !== expected.length || actual.some((name) => !expected.includes(name))) throw new Error('Expected exactly six native npm packages and their six standalone archives');
  for (const platform of platforms) {
    run('python3', ['scripts/verify-package.py', nativeFile(platform), platform]);
    run('python3', ['scripts/verify-archive.py', archiveFile(platform), nativeFile(platform), platform]);
  }
}
async function published(name, timeoutMs = 15000) {
  const url = new URL(`https://registry.npmjs.org/${encodeURIComponent(name)}/${cli.version}`);
  url.searchParams.set('visibility', crypto.randomUUID());
  const response = await fetch(url, {
    signal: AbortSignal.timeout(timeoutMs), cache: 'no-store',
    headers: { 'Cache-Control': 'no-cache', Pragma: 'no-cache' },
  });
  if (response.status === 404) return null;
  if (!response.ok) {
    const error = new Error(`Registry check failed: ${response.status}`);
    error.retryable = response.status === 429 || response.status >= 500;
    throw error;
  }
  const manifest = await response.json();
  if (manifest.name !== name || manifest.version !== cli.version) throw new Error('Registry package name/version mismatch');
  if (!manifest.dist || typeof manifest.dist.integrity !== 'string' || typeof manifest.dist.tarball !== 'string') throw new Error(`Registry integrity missing for ${name}`);
  return manifest;
}
function integrityMatches(bytes, integrity) {
  const values = integrity.split(/\s+/).filter((value) => value.startsWith('sha512-'));
  if (!values.length) throw new Error('Registry tarball must have SHA-512 integrity');
  const actual = crypto.createHash('sha512').update(bytes).digest('base64');
  return values.some((value) => value.slice(7) === actual);
}
async function waitForPublished(name, file, timeoutMs = 180000) {
  const deadline = Date.now() + timeoutMs;
  const bytes = fs.readFileSync(file);
  let waiting = false;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const manifest = await published(name, Math.max(1, Math.min(15000, deadline - Date.now())));
      if (manifest) {
        if (!integrityMatches(bytes, manifest.dist.integrity)) throw new Error(`Published package integrity differs: ${name}`);
        return manifest;
      }
    } catch (error) {
      // Retry visibility/read failures only. A name/version/integrity mismatch is fatal.
      if (!error.retryable && !['TimeoutError', 'AbortError', 'TypeError'].includes(error.name)) throw error;
      lastError = error;
    }
    if (!waiting) {
      console.log(`Waiting up to ${Math.ceil(timeoutMs / 1000)}s for npm visibility: ${name}@${cli.version}`);
      waiting = true;
    }
    const remaining = deadline - Date.now();
    if (remaining > 0) await new Promise((resolve) => setTimeout(resolve, Math.min(3000, remaining)));
  }
  throw new Error(`npm visibility timed out for ${name}@${cli.version}; do not republish blindly. Resume after registry propagation.${lastError ? ` Last read error: ${lastError.message}` : ''}`);
}
async function recoverPublished(manifest, file, platform) {
  if (integrityMatches(fs.readFileSync(file), manifest.dist.integrity)) return;
  const url = new URL(manifest.dist.tarball);
  if (url.origin !== 'https://registry.npmjs.org' || url.username || url.password) throw new Error('Unexpected registry tarball URL');
  const response = await fetch(url, { signal: AbortSignal.timeout(60000), redirect: 'error' });
  if (!response.ok) throw new Error(`Registry tarball download failed: ${response.status}`);
  const chunks = [];
  let size = 0;
  for await (const chunk of response.body) {
    size += chunk.length;
    if (size > 40 * 1024 * 1024) throw new Error('Registry tarball too large');
    chunks.push(chunk);
  }
  const bytes = Buffer.concat(chunks);
  if (!integrityMatches(bytes, manifest.dist.integrity)) throw new Error(`Registry tarball integrity mismatch: ${manifest.name}`);
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'harness-registry-'));
  try {
    const candidate = path.join(temp, 'package.tgz');
    fs.writeFileSync(candidate, bytes);
    run('python3', ['scripts/verify-package.py', candidate, platform]);
    fs.copyFileSync(candidate, file);
    console.log(`Recovered exact published bytes for ${manifest.name}@${cli.version}`);
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
}
function checksum(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}
function githubRelease() {
  const tarball = path.join(dir, `codex-harness-${cli.version}.tgz`);
  run('python3', ['scripts/verify-package.py', tarball, 'cli']);
  const files = [...platforms.flatMap((platform) => [nativeFile(platform), archiveFile(platform)]), tarball].sort();
  const sumsFile = path.join(dir, 'SHA256SUMS');
  fs.writeFileSync(sumsFile, files.map((file) => `${checksum(file)}  ${path.basename(file)}\n`).join(''));
  files.push(sumsFile);
  const existing = spawnSync('gh', ['api', `repos/ErzenXz/codex-harness/releases/tags/${tag}`], { encoding: 'utf8', maxBuffer: 4 * 1024 * 1024 });
  if (existing.status !== 0) {
    if (!/HTTP 404/.test(existing.stderr || '')) throw new Error(`Cannot inspect GitHub release: ${existing.stderr}`);
    run('gh', ['release', 'create', tag, ...files, '--verify-tag', '--title', `Codex Harness ${cli.version}`, '--notes-file', 'CHANGELOG.md']);
    return;
  }
  const release = JSON.parse(existing.stdout);
  if (release.tag_name !== tag) throw new Error('GitHub release tag mismatch');
  const expected = new Map(files.map((file) => [path.basename(file), file]));
  const missing = new Map(expected);
  for (const asset of release.assets) {
    const file = expected.get(asset.name);
    if (!file || !missing.has(asset.name)) throw new Error(`Unexpected/duplicate GitHub release asset: ${asset.name}`);
    let digest = asset.digest;
    if (!digest) {
      if (!Number.isSafeInteger(asset.id) || asset.size > 40 * 1024 * 1024) throw new Error('Invalid existing release asset');
      const data = spawnSync('gh', ['api', `repos/ErzenXz/codex-harness/releases/assets/${asset.id}`, '-H', 'Accept: application/octet-stream'], { maxBuffer: 40 * 1024 * 1024 });
      if (data.status !== 0) throw new Error(`Cannot verify GitHub asset: ${asset.name}`);
      digest = `sha256:${crypto.createHash('sha256').update(data.stdout).digest('hex')}`;
    }
    if (digest !== `sha256:${checksum(file)}`) throw new Error(`Existing GitHub asset differs; refusing to overwrite ${asset.name}`);
    missing.delete(asset.name);
  }
  if (missing.size) run('gh', ['release', 'upload', tag, ...missing.values()]);
  console.log(`GitHub release ${tag} assets verified.`);
}
async function main() {
  preflight();
  if (action === 'validate') return;
  if (action === 'publish') {
    // Resolve all immutable existing packages before writing anything to npm.
    const manifests = new Map();
    for (const platform of platforms) {
      const manifest = await published(`@erzenx/codex-harness-${platform}`);
      manifests.set(platform, manifest);
      if (manifest) await recoverPublished(manifest, nativeFile(platform), platform);
    }
    // Rebuilt standalone binaries must match any already-published native binary.
    preflight();
    const out = path.join(process.env.RUNNER_TEMP || os.tmpdir(), `cli-package-${crypto.randomUUID()}`);
    const tarball = path.join(dir, `codex-harness-${cli.version}.tgz`);
    try {
      stage({ out, platform: 'cli' });
      run('pnpm', ['--dir', out, 'pack', '--out', tarball]);
    } finally {
      fs.rmSync(out, { recursive: true, force: true });
    }
    run('python3', ['scripts/verify-package.py', tarball, 'cli']);
    const rootManifest = await published(cli.name);
    if (rootManifest) await recoverPublished(rootManifest, tarball, 'cli');
    for (const platform of platforms) {
      if (!manifests.get(platform)) run('pnpm', ['publish', nativeFile(platform), '--access', 'public', '--provenance', '--ignore-scripts', '--no-git-checks']);
    }
    // Publishing is attempted once per package. Metadata propagation can lag the
    // successful write; wait for every native before exposing the launcher.
    await Promise.all(platforms.map((platform) => waitForPublished(`@erzenx/codex-harness-${platform}`, nativeFile(platform))));
    if (!rootManifest) run('pnpm', ['publish', tarball, '--access', 'public', '--provenance', '--ignore-scripts', '--no-git-checks']);
    await waitForPublished(cli.name, tarball);
  } else {
    // An independently resumed GitHub step must use the actual npm bytes.
    for (const platform of platforms) {
      const manifest = await published(`@erzenx/codex-harness-${platform}`);
      if (!manifest) throw new Error(`Native package not published: ${platform}`);
      await recoverPublished(manifest, nativeFile(platform), platform);
    }
    const tarball = path.join(dir, `codex-harness-${cli.version}.tgz`);
    const manifest = await published(cli.name);
    if (!manifest) throw new Error('Launcher package not published');
    await recoverPublished(manifest, tarball, 'cli');
    preflight();
    githubRelease();
  }
}
main().catch((error) => { console.error(error.message); process.exitCode = 1; });

# Releases

Development uses the pinned current pnpm in `package.json` and the Rust lockfile.
There are no JavaScript development dependencies. `pnpm-workspace.yaml` enforces
a one-day dependency age, strict build-script approval and no exotic transitive
sources. npm packages themselves have no lifecycle scripts or runtime dependencies
other than six exact-version optional native packages; the installer selects one.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
python3 tests/stdio.py
pnpm test:package
pnpm test:install
```

CI runs native Rust/ACP/launcher/packed-install tests on macOS/Linux/Windows,
arm64/x64. Native desktop-driver fixtures are Linux-only. Linux releases compile
against musl for distribution across glibc/musl hosts. Builds and package staging
stay in runner temporary directories; local validation should also use scratch.

## Publish

1. Keep the Rust, development and `npm/cli/package.json` versions aligned,
   including every optional dependency pin. Update the changelog and measurements.
   After changing Cargo.lock, refresh bundled attributions offline with
   `python3 scripts/licenses.py`.
2. Verify all six CI jobs, then tag the tested commit `vVERSION` and push the tag.
3. The release workflow repeats verification, checks archive names/manifests,
   executable signatures and a strict five-file package allowlist, and publishes
   all six native packages before the launcher. It verifies registry visibility
   after every publish. npm's [publish-time scanning](https://github.blog/changelog/2026-07-28-npm-publish-time-malware-scanning-and-dual-use-metadata/)
   can delay availability for several minutes after an accepted upload; the
   visibility window is 20 minutes. Matching existing versions make a partial
   publish resumable. If that window expires, check accepted uploads in the logs
   and wait for their exact versions to become visible before rerunning the failed
   publish job. Do not republish accepted versions blindly.
4. It publishes checksummed native downloads and npm tarballs as a GitHub release.

First publication needs authenticated maintainer access to create each package.
Use the normal npm browser/passkey authorization when prompted. Never commit an
npm token. After bootstrapping, configure each of the seven packages to trust
`ErzenXz/codex-harness`, workflow `release.yml`, for npm trusted publishing.
Subsequent releases use GitHub OIDC with provenance and no stored publishing token.
The `npm trust` account-management command may be needed to configure this;
installation, packing and publishing use pnpm.

Release artifacts expire after seven days in normal CI. Download them from the
verified run for a first local publication, validate them with
`python3 scripts/verify-package.py TARBALL PLATFORM`, publish native tarballs with
`pnpm publish TARBALL --access public --ignore-scripts --no-git-checks`, then stage
and publish the CLI using `node scripts/package.cjs --platform cli --out ABS_DIR`.
Do not publish a launcher before every exact native version is publicly visible.

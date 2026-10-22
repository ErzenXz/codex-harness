"""Read-only verification of public npm/GitHub release payloads (stdlib only).

Run from any directory: python3 scripts/verify-release.py --version 0.7.0 --out REPORT.json
Downloads live only in a temporary directory and are removed on success or failure.
"""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

REPO = Path(__file__).resolve().parent.parent
PLATFORMS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-arm64", "win32-x64")
HOSTS = frozenset(("registry.npmjs.org", "api.github.com", "github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com"))
MAX_ASSET = 40 * 1024 * 1024


def safe_url(url, hosts=HOSTS):
    if not isinstance(url, str):
        raise ValueError("Download URL must be text")
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "https" or parsed.hostname not in hosts or parsed.port not in (None, 443)
            or parsed.username or parsed.password or parsed.fragment):
        raise ValueError("Unexpected download origin: " + parsed.netloc)
    return url


class SafeRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, new_url):
        safe_url(new_url)
        # urllib never receives an authentication header in this public verifier.
        return super().redirect_request(request, fp, code, message, headers, new_url)


class Reader:
    def __init__(self, timeout):
        self.deadline = time.monotonic() + timeout
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), SafeRedirect())

    def download(self, url, limit, path=None):
        safe_url(url)
        attempts = 0
        while True:
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Public release visibility deadline reached; wait for CDN propagation rather than republishing")
            # Cache-bust the initial public URL only; signed redirects remain intact.
            parts = urllib.parse.urlsplit(url)
            query = urllib.parse.parse_qsl(parts.query, keep_blank_values=True)
            query.append(("piko_verify", str(time.time_ns())))
            request_url = urllib.parse.urlunsplit(parts._replace(query=urllib.parse.urlencode(query)))
            request = urllib.request.Request(request_url, headers={
                "User-Agent": "Piko-release-verifier/1", "Cache-Control": "no-cache", "Pragma": "no-cache",
                "Accept": "application/vnd.github+json" if parts.hostname == "api.github.com" else "*/*",
            })
            try:
                with self.opener.open(request, timeout=min(30, remaining)) as response:
                    safe_url(response.url)
                    length = response.headers.get("Content-Length")
                    if length is not None and int(length) > limit:
                        raise ValueError("Download exceeds size limit")
                    total = 0
                    chunks = []
                    output = path.open("wb") if path else None
                    try:
                        while True:
                            if time.monotonic() >= self.deadline:
                                raise TimeoutError("Public release verification deadline reached")
                            chunk = response.read(min(64 * 1024, limit + 1 - total))
                            if not chunk:
                                break
                            total += len(chunk)
                            if total > limit:
                                raise ValueError("Download exceeds size limit")
                            if output:
                                output.write(chunk)
                            else:
                                chunks.append(chunk)
                    finally:
                        if output:
                            output.close()
                    if length is not None and total != int(length):
                        raise OSError("Incomplete public payload download")
                    return total if path else b"".join(chunks)
            except urllib.error.HTTPError as error:
                if error.code not in (404, 408, 429) and not 500 <= error.code <= 599:
                    raise
                reason = "HTTP " + str(error.code)
                error.close()
            except (urllib.error.URLError, TimeoutError, ConnectionError, OSError) as error:
                reason = type(error).__name__
            attempts += 1
            if attempts == 1:
                print(f"Waiting for public CDN visibility ({reason}): {parts.path}", file=sys.stderr, flush=True)
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Public payload unavailable before verification deadline: " + parts.path)
            time.sleep(min(15, 2 + attempts, remaining))

    def json(self, url):
        return json.loads(self.download(url, 4 * 1024 * 1024))


def digest(path, algorithm="sha256"):
    result = hashlib.new(algorithm)
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(64 * 1024), b""):
            result.update(chunk)
    return result.digest()


def verify_script(script, arguments, cwd):
    result = subprocess.run([sys.executable, "-I", str(REPO / "scripts" / script), *map(str, arguments)],
                            cwd=cwd, capture_output=True, text=True, timeout=60, check=False)
    if result.returncode:
        raise ValueError(script + " rejected public payload: " + (result.stderr or result.stdout).strip()[-4000:])


def check(condition, message):
    if not condition:
        raise ValueError(message)


def verify(version, timeout):
    reader = Reader(timeout)
    with tempfile.TemporaryDirectory(prefix="piko-public-verification-") as directory:
        work = Path(directory)
        # Existing validators deliberately compare against the trusted checkout's
        # launcher contract. Isolate a version-adjusted expected manifest so this
        # verifier can also validate an older release without modifying the repo.
        cli = json.loads((REPO / "npm/cli/package.json").read_text())
        cli["version"] = version
        cli["optionalDependencies"] = {"@erzenx/piko-" + platform: version for platform in PLATFORMS}
        (work / "npm/cli").mkdir(parents=True)
        (work / "npm/cli/package.json").write_text(json.dumps(cli))
        npm = {}
        paths = {}
        for platform in (*PLATFORMS, "cli"):
            name = "@erzenx/piko" + ("" if platform == "cli" else "-" + platform)
            manifest = reader.json("https://registry.npmjs.org/" + urllib.parse.quote(name, safe="") + "/" + urllib.parse.quote(version, safe=""))
            check(manifest.get("name") == name and manifest.get("version") == version, "Registry identity mismatch")
            dist = manifest.get("dist", {})
            check(isinstance(dist, dict), "Invalid registry distribution metadata")
            url = safe_url(dist.get("tarball", ""), frozenset(("registry.npmjs.org",)))
            integrity = dist.get("integrity", "")
            check(isinstance(integrity, str), "Invalid npm integrity metadata")
            file_name = f"erzenx-piko-{version}.tgz" if platform == "cli" else f"native-{platform}.tgz"
            path = work / file_name
            reader.download(url, MAX_ASSET, path)
            actual = "sha512-" + base64.b64encode(digest(path, "sha512")).decode("ascii")
            check(actual in integrity.split(), "npm SHA-512 mismatch for " + name)
            verify_script("verify-package.py", (path, platform), work)
            entry = {"name": name, "version": version, "tarballBytes": path.stat().st_size,
                     "integrity": actual, "tarballSha256": digest(path).hex()}
            if platform != "cli":
                binary = "package/bin/piko" + (".exe" if platform.startswith("win32") else "")
                with tarfile.open(path, "r:gz") as package:
                    member = package.getmember(binary)
                    entry["binaryBytes"] = member.size
                    entry["binarySha256"] = hashlib.file_digest(package.extractfile(member), "sha256").hexdigest()
            attestations = dist.get("attestations")
            if isinstance(attestations, dict):
                entry["attestations"] = attestations
            npm[platform] = entry
            paths[file_name] = path
            print("Verified public npm " + name + "@" + version, file=sys.stderr, flush=True)

        tag = "v" + version
        release = reader.json("https://api.github.com/repos/ErzenXz/piko/releases/tags/" + urllib.parse.quote(tag, safe=""))
        check(release.get("tag_name") == tag and not release.get("draft"), "GitHub release identity/draft mismatch")
        archive_names = {platform: f"piko-{version}-{platform}." + ("zip" if platform.startswith("win32") else "tar.gz") for platform in PLATFORMS}
        expected = set(paths) | set(archive_names.values()) | {"SHA256SUMS"}
        assets = {}
        for asset in release.get("assets", []):
            name, size = asset.get("name"), asset.get("size")
            check(name in expected and name not in assets, "Unexpected/duplicate release asset")
            check(type(size) is int and 0 < size <= MAX_ASSET, "Invalid release asset size")
            assets[name] = asset
        check(set(assets) == expected and len(assets) == 14, "Expected exactly 14 public release assets")
        public_paths = {}
        for name, asset in assets.items():
            url = safe_url(asset.get("browser_download_url", ""), frozenset(("github.com",)))
            parsed = urllib.parse.urlsplit(url)
            expected_path = "/ErzenXz/piko/releases/download/" + tag + "/" + name
            check(urllib.parse.unquote(parsed.path) == expected_path, "Unexpected GitHub asset path")
            path = work / ("github-" + name)
            reader.download(url, 64 * 1024 if name == "SHA256SUMS" else MAX_ASSET, path)
            check(path.stat().st_size == asset["size"], "GitHub asset size mismatch: " + name)
            github_digest = asset.get("digest")
            if github_digest:
                check(github_digest == "sha256:" + digest(path).hex(), "GitHub asset digest mismatch: " + name)
            public_paths[name] = path
        sums = {}
        for line in public_paths["SHA256SUMS"].read_text(encoding="ascii").splitlines():
            match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
            check(match is not None, "Invalid SHA256SUMS line")
            checksum, name = match.groups()
            check(name in expected - {"SHA256SUMS"} and name not in sums, "Unexpected/duplicate checksummed asset")
            sums[name] = checksum
        check(set(sums) == expected - {"SHA256SUMS"} and len(sums) == 13, "Expected exactly 13 payload checksums")
        for name, checksum in sums.items():
            check(digest(public_paths[name]).hex() == checksum, "GitHub SHA256SUMS mismatch: " + name)
        for name, npm_path in paths.items():
            check(digest(npm_path) == digest(public_paths[name]), "GitHub/npm tarball mismatch: " + name)
        for platform, name in archive_names.items():
            verify_script("verify-archive.py", (public_paths[name], paths[f"native-{platform}.tgz"], platform), work)
        return {"version": version, "verifiedAt": datetime.now(timezone.utc).isoformat(), "npm": npm,
                "github": {"url": release.get("html_url"), "tag": tag, "publishedAt": release.get("published_at"),
                           "assets": {name: {"bytes": assets[name]["size"], "sha256": digest(path).hex()} for name, path in sorted(public_paths.items())}},
                "checks": {"npmSha512": 7, "packageAllowlists": 7, "releaseAssets": 14,
                           "checksummedPayloads": 13, "registryGithubTarballParity": 7, "standaloneNativeParity": 6}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--timeout", type=int, default=1200, help="Global visibility deadline, 1–1200 seconds (default: 1200)")
    arguments = parser.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", arguments.version):
        parser.error("Invalid semantic release version")
    if not 1 <= arguments.timeout <= 1200:
        parser.error("--timeout must be between 1 and 1200 seconds")
    report = verify(arguments.version, arguments.timeout)
    arguments.out.parent.mkdir(parents=True, exist_ok=True)
    # Never leave a success-looking partial report after an interrupted write.
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", prefix=".piko-verification-", dir=arguments.out.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            json.dump(report, output, indent=2)
            output.write("\n")
            output.flush()
            output.close()
            temporary.replace(arguments.out)
        finally:
            temporary.unlink(missing_ok=True)
    print(json.dumps({"version": arguments.version, "verified": True, "checks": report["checks"], "report": str(arguments.out.resolve())}, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError, subprocess.SubprocessError) as error:
        print("Release verification failed: " + str(error), file=sys.stderr)
        sys.exit(1)

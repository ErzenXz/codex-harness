"""Validate published payloads before registry writes. Reject unexpected data and links."""
import argparse
import json
from pathlib import Path
import tarfile

p = argparse.ArgumentParser()
p.add_argument("tarball", type=Path)
p.add_argument("platform")
a = p.parse_args()
cli = json.loads(Path("npm/cli/package.json").read_text())
with tarfile.open(a.tarball, "r:gz") as tar:
    files = {}
    for entry in tar.getmembers():
        if entry.isdir():
            continue
        if not entry.isfile() or entry.name in files:
            raise SystemExit("Invalid package member")
        if entry.size > 32 * 1024 * 1024:
            raise SystemExit("Unexpectedly large payload")
        files[entry.name] = entry
    executable = "bin/codex-harness.cjs" if a.platform == "cli" else "bin/codex-harness" + (".exe" if a.platform.startswith("win32") else "")
    expected = {"package/" + f for f in ["package.json", "LICENSE", "THIRD_PARTY_NOTICES.md", "README.md", executable]}
    if set(files) != expected:
        raise SystemExit(f"Unexpected files: {set(files) ^ expected}")
    manifest = json.load(tar.extractfile("package/package.json"))
    name = cli["name"] if a.platform == "cli" else "@erzenx/codex-harness-" + a.platform
    assert manifest["name"] == name and manifest["version"] == cli["version"], "Package name/version mismatch"
    assert not manifest.get("scripts") and not manifest.get("dependencies"), "Unexpected runtime scripts/dependencies"
    if a.platform == "cli":
        assert manifest["optionalDependencies"] == cli["optionalDependencies"]
    else:
        os, cpu = a.platform.split("-")
        assert manifest["os"] == [os] and manifest["cpu"] == [cpu]
        binary = files["package/" + executable]
        assert binary.size > 1024 * 1024, "Missing/truncated native binary"
        magic = tar.extractfile(binary).read(4)
        if os == "linux": assert magic == b"\x7fELF"
        elif os == "win32": assert magic[:2] == b"MZ"
        else: assert magic in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe")
        if os != "win32":
            assert binary.mode & 0o111, "Missing executable permission"
print(f"Verified {manifest['name']}@{manifest['version']}")

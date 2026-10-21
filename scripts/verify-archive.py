"""Verify standalone archive payloads match their corresponding npm binary."""
import hashlib
import json
from pathlib import Path
import sys
import tarfile
import zipfile

archive, native, platform = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
binary = "piko.exe" if platform.startswith("win32-") else "piko"
expected = {binary, "LICENSE", "THIRD_PARTY_NOTICES.md"}
limit = 32 * 1024 * 1024

with tarfile.open(native, "r:gz") as package:
    manifest = json.load(package.extractfile("package/package.json"))
    if manifest["name"] != "@erzenx/piko-" + platform:
        raise SystemExit("Native package platform mismatch")
    reference = {}
    for name in expected:
        member = package.getmember("package/" + ("bin/" if name == binary else "") + name)
        if not member.isfile() or member.size > limit:
            raise SystemExit("Invalid native package payload")
        reference[name] = hashlib.sha256(package.extractfile(member).read()).digest()


def check(name, data):
    if name not in reference or hashlib.sha256(data).digest() != reference[name]:
        raise SystemExit("Standalone archive differs from npm package: " + name)


if platform.startswith("win32-"):
    with zipfile.ZipFile(archive) as payload:
        entries = payload.infolist()
        if len(entries) != len(expected) or {entry.filename for entry in entries} != expected:
            raise SystemExit("Unexpected standalone ZIP members")
        for entry in entries:
            mode = entry.external_attr >> 16
            if entry.is_dir() or (mode & 0o170000) == 0o120000 or entry.file_size > limit:
                raise SystemExit("Invalid standalone ZIP payload")
            check(entry.filename, payload.read(entry))
else:
    with tarfile.open(archive, "r:gz") as payload:
        entries = payload.getmembers()
        if len(entries) != len(expected) or {entry.name for entry in entries} != expected:
            raise SystemExit("Unexpected standalone tar members")
        for entry in entries:
            if not entry.isfile() or entry.size > limit:
                raise SystemExit("Invalid standalone tar payload")
            if entry.name == binary and not entry.mode & 0o111:
                raise SystemExit("Standalone binary is not executable")
            check(entry.name, payload.extractfile(entry).read())
print("Verified standalone archive for " + platform)

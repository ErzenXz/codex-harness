"""Create a small native release archive; never includes local state or build trees."""
import argparse
import json
from pathlib import Path
import tarfile
import zipfile

p = argparse.ArgumentParser()
p.add_argument("--binary", required=True, type=Path)
p.add_argument("--platform", required=True)
p.add_argument("--out", required=True, type=Path)
a = p.parse_args()
version = json.loads(Path("package.json").read_text())["version"]
a.out.mkdir(parents=True, exist_ok=True)
name = f"codex-harness-{version}-{a.platform}"
files = [(a.binary, "codex-harness.exe" if a.platform.startswith("win32") else "codex-harness"),
         (Path("LICENSE"), "LICENSE"), (Path("THIRD_PARTY_NOTICES.md"), "THIRD_PARTY_NOTICES.md")]
if a.platform.startswith("win32"):
    with zipfile.ZipFile(a.out / (name + ".zip"), "w", zipfile.ZIP_DEFLATED) as archive:
        for source, target in files:
            archive.write(source, target)
else:
    with tarfile.open(a.out / (name + ".tar.gz"), "w:gz") as archive:
        for source, target in files:
            archive.add(source, arcname=target)

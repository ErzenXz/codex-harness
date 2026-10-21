"""Create a small native release archive; never includes local state or build trees."""
import argparse
import gzip
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
            entry = zipfile.ZipInfo(target, date_time=(1980, 1, 1, 0, 0, 0))
            entry.create_system = 3
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = (0o100755 if source == a.binary else 0o100644) << 16
            archive.writestr(entry, source.read_bytes())
else:
    with (a.out / (name + ".tar.gz")).open("wb") as output:
        with gzip.GzipFile(filename="", fileobj=output, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for source, target in files:
                    entry = tarfile.TarInfo(target)
                    entry.size = source.stat().st_size
                    entry.mode = 0o755 if source == a.binary else 0o644
                    with source.open("rb") as content:
                        archive.addfile(entry, content)

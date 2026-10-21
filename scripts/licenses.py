"""Refresh bundled dependency notices from the locked Cargo source cache (offline)."""
import hashlib
import json
from pathlib import Path
import subprocess

targets = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]
selected = set()
for target in targets:
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--offline", "--locked", "--format-version=1", "--filter-platform", target]))
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = [metadata["resolve"]["root"]]
    seen = set()
    while pending:
        identifier = pending.pop()
        if identifier in seen:
            continue
        seen.add(identifier)
        for dependency in nodes[identifier]["deps"]:
            if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])
    selected.update(seen)
groups = {}
packages = []
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["name"] == "codex-harness" or package["id"] not in selected:
        continue
    root = Path(package["manifest_path"]).parent
    files = [p for p in root.iterdir() if p.is_file() and p.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE"))]
    if package.get("license_file"):
        file = root / package["license_file"]
        if file.is_file() and file not in files:
            files.append(file)
    if not files:
        for directory in (root / "LICENSES", root / "licenses"):
            if directory.is_dir():
                files.extend(p for p in directory.rglob("*") if p.is_file())
    if not files:
        raise SystemExit(f"Missing license text for {package['name']}")
    identifiers = []
    for file in sorted(files):
        text = file.read_text(encoding="utf-8").strip()
        identifier = hashlib.sha256(text.encode()).hexdigest()[:16]
        groups.setdefault(identifier, text)
        identifiers.append(identifier)
    packages.append(f"- {package['name']} {package['version']} ({package.get('license', 'see license text')}): " + ", ".join(f"[text {i}](#license-{i})" for i in identifiers))
notice = Path("THIRD_PARTY_NOTICES.md")
marker = "<!-- dependency-licenses:start -->"
header = notice.read_text().split(marker)[0].rstrip()
content = header + "\n\n" + marker + "\n\n# Locked dependency licenses\n\nGenerated offline by `python3 scripts/licenses.py`. Includes locked runtime/build dependencies across all six supported release targets, so some entries do not ship in a given binary. Identical texts are included once.\n\n" + "\n".join(packages)
for identifier, text in groups.items():
    content += f'\n\n<a id="license-{identifier}"></a>\n\n## License {identifier}\n\n```text\n{text}\n```'
notice.write_text(content + "\n")
print(f"Included {len(packages)} dependency attributions and {len(groups)} distinct license texts.")

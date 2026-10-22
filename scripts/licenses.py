"""Generate/check bundled licenses from locked Cargo metadata and cached sources."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

TARGETS = (
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
)
MARKER = "<!-- dependency-licenses:start -->"


def selected_packages(metadata_sets):
    """Union both selected IDs AND package records; the final target is not complete."""
    selected, roots, packages = set(), set(), {}
    for metadata in metadata_sets:
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        root = metadata["resolve"]["root"]
        roots.add(root)
        packages.update((package["id"], package) for package in metadata["packages"])
        pending, seen = [root], set()
        while pending:
            identifier = pending.pop()
            if identifier in seen:
                continue
            seen.add(identifier)
            if identifier not in nodes:
                raise ValueError(f"Selected dependency has no resolved node: {identifier}")
            for dependency in nodes[identifier]["deps"]:
                if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                    pending.append(dependency["pkg"])
        selected.update(seen)
    missing = selected - packages.keys()
    if missing:
        raise ValueError("Selected dependencies have no package metadata: " + ", ".join(sorted(missing)))
    return sorted((packages[identifier] for identifier in selected - roots),
                  key=lambda package: (package["name"], package["version"], package["id"]))


def attributions(packages):
    groups, entries = {}, []
    for package in packages:
        root = Path(package["manifest_path"]).parent
        files = [path for path in root.iterdir() if path.is_file() and
                 path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE"))]
        if package.get("license_file"):
            path = root / package["license_file"]
            if path.is_file() and path not in files:
                files.append(path)
        if not files:
            for directory in (root / "LICENSES", root / "licenses"):
                if directory.is_dir():
                    files.extend(path for path in directory.rglob("*") if path.is_file())
        if not files:
            raise ValueError(f"Missing license text for {package['name']} {package['version']}")
        identifiers = []
        for path in sorted(files):
            text = path.read_text(encoding="utf-8").strip()
            if not text:
                raise ValueError(f"Empty license text: {path}")
            identifier = hashlib.sha256(text.encode()).hexdigest()[:16]
            if identifier in groups and groups[identifier] != text:
                raise ValueError("License identifier collision")
            groups[identifier] = text
            identifiers.append(identifier)
        entries.append(f"- {package['name']} {package['version']} ({package.get('license') or 'see license text'}): " +
                       ", ".join(f"[text {identifier}](#license-{identifier})" for identifier in identifiers))
    return entries, groups


def license_block(identifier, text):
    return f'<a id="license-{identifier}"></a>\n\n## License {identifier}\n\n```text\n{text}\n```'


def check_coverage(content, entries, groups):
    """A target-specific CI check allows additional notices for other platforms."""
    if MARKER not in content:
        raise ValueError("Dependency license section is missing")
    section = content.split(MARKER, 1)[1]
    lines = set(section.splitlines())
    missing = [entry for entry in entries if entry not in lines]
    missing.extend(f"License text {identifier}" for identifier, text in groups.items()
                   if license_block(identifier, text) not in section)
    if missing:
        raise ValueError("Committed notices are incomplete; run python3 scripts/licenses.py:\n" + "\n".join(missing))


def render(content, entries, groups):
    header = content.split(MARKER, 1)[0].rstrip()
    result = (header + "\n\n" + MARKER + "\n\n# Locked dependency licenses\n\n"
              "Generated offline by `python3 scripts/licenses.py`. Includes locked runtime/build dependencies "
              "across all six supported release targets, so some entries do not ship in a given binary. "
              "Identical texts are included once.\n\n" + "\n".join(entries))
    for identifier, text in groups.items():
        result += "\n\n" + license_block(identifier, text)
    return result + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Verify committed coverage without writing notices")
    parser.add_argument("--target", action="append", choices=TARGETS,
                        help="Check only this cached release target (repeatable; --check only)")
    args = parser.parse_args()
    if args.target and not args.check:
        parser.error("--target is only allowed with --check; regeneration must include all six targets")
    metadata_sets = (json.loads(subprocess.check_output([
        "cargo", "metadata", "--offline", "--locked", "--format-version=1", "--filter-platform", target,
    ])) for target in args.target or TARGETS)
    entries, groups = attributions(selected_packages(metadata_sets))
    notice = Path("THIRD_PARTY_NOTICES.md")
    content = notice.read_text(encoding="utf-8")
    if args.check:
        check_coverage(content, entries, groups)
    else:
        notice.write_text(render(content, entries, groups), encoding="utf-8")
    print(f"{'Verified' if args.check else 'Included'} {len(entries)} dependency attributions and {len(groups)} distinct license texts.")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error

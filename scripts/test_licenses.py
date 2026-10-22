"""License coverage regressions using tiny synthetic target metadata, no Cargo/network."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("licenses", Path(__file__).with_name("licenses.py"))
licenses = importlib.util.module_from_spec(spec)
spec.loader.exec_module(licenses)


def metadata(packages, dependencies):
    return {"packages": packages, "resolve": {"root": "root", "nodes": [
        {"id": "root", "deps": [{"pkg": identifier, "dep_kinds": [{"kind": kind}]}
                                  for identifier, kind in dependencies]},
        *({"id": package["id"], "deps": []} for package in packages if package["id"] != "root"),
    ]}}


class LicenseTests(unittest.TestCase):
    def test_target_union_keeps_unix_only_metadata_and_full_text(self):
        with tempfile.TemporaryDirectory() as directory:
            packages = []
            for name in ("unix-only", "windows-only"):
                root = Path(directory) / name
                root.mkdir()
                (root / "LICENSE").write_text(f"{name} copyright and license\n", encoding="utf-8")
                packages.append({"id": name, "name": name, "version": "1.0.0", "license": "MIT",
                                 "manifest_path": str(root / "Cargo.toml")})
            root = {"id": "root"}
            unix = metadata([root, packages[0]], [("unix-only", None)])
            windows = metadata([root, packages[1]], [("windows-only", "build")])
            selected = licenses.selected_packages([unix, windows])
            self.assertEqual([package["name"] for package in selected], ["unix-only", "windows-only"])
            entries, groups = licenses.attributions(selected)
            content = licenses.render("Header\n", entries, groups)
            licenses.check_coverage(content, entries, groups)
            # Target-specific CI accepts the complete cross-platform document.
            subset, texts = licenses.attributions(licenses.selected_packages([unix]))
            licenses.check_coverage(content, subset, texts)
            with self.assertRaisesRegex(ValueError, "incomplete"):
                licenses.check_coverage(content.replace(entries[0], ""), entries, groups)
            with self.assertRaisesRegex(ValueError, "License text"):
                licenses.check_coverage(content.replace("unix-only copyright and license", "tampered"), entries, groups)

    def test_selected_id_without_package_record_fails_closed(self):
        data = metadata([{"id": "root"}, {"id": "missing"}], [("missing", None)])
        data["packages"] = [{"id": "root"}]
        with self.assertRaisesRegex(ValueError, "no package metadata.*missing"):
            licenses.selected_packages([data])

    def test_dev_only_dependency_is_not_selected(self):
        root = {"id": "root"}
        dev = {"id": "dev", "name": "dev", "version": "1"}
        self.assertEqual(licenses.selected_packages([metadata([root, dev], [("dev", "dev")])]), [])

    def test_empty_license_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "LICENSE").write_text("\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Empty license"):
                licenses.attributions([{"name": "empty", "version": "1", "manifest_path": str(root / "Cargo.toml")}])


if __name__ == "__main__":
    unittest.main()

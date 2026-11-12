"""Release gates must reject incomplete, corrupted and incorrectly labeled packages."""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


matrix = module("package_matrix", "scripts/ci/matrix.py")
release = module("verify_release", "scripts/ci/verify-release.py")


class MatrixTests(unittest.TestCase):
    def setUp(self):
        self.config = json.loads((ROOT / ".github/package-targets.json").read_text())
        self.manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())

    def test_every_declared_pg_has_every_native_platform(self):
        targets = matrix.select_matrix(self.config, self.manifest)["include"]
        self.assertEqual(len(targets), 30)
        for pg in range(13, 19):
            self.assertEqual(len([target for target in targets if target["pg"] == pg]), 5)

    def test_single_target_dispatch(self):
        targets = matrix.select_matrix(self.config, self.manifest, "windows-2022-x86_64", "13")["include"]
        self.assertEqual(targets, [{"platform": "windows-2022", "arch": "x86_64", "runner": "windows-2022", "pg": 13}])

    def test_new_cargo_pg_feature_cannot_silently_miss_release(self):
        self.manifest["features"]["pg19"] = []
        with self.assertRaisesRegex(ValueError, "differ from Cargo"):
            matrix.select_matrix(self.config, self.manifest)

    def test_unknown_filters_and_duplicate_platforms_fail(self):
        for platform, postgres in [("windows-2022-arm64", "all"), ("all", "19")]:
            with self.assertRaises(ValueError):
                matrix.select_matrix(self.config, self.manifest, platform, postgres)
        self.config["platforms"].append(copy.deepcopy(self.config["platforms"][0]))
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            matrix.select_matrix(self.config, self.manifest)

    def test_release_tag_cannot_filter_out_a_failing_platform(self):
        version = self.manifest["package"]["version"]
        environment = dict(os.environ, GITHUB_EVENT_NAME="push", GITHUB_REF_TYPE="tag",
                           GITHUB_REF_NAME=f"v{version}", GP_PLATFORM_FILTER="windows-2022-x86_64",
                           GP_POSTGRES_FILTER="13")
        environment.pop("GITHUB_OUTPUT", None)
        result = subprocess.run([sys.executable, ROOT / "scripts/ci/matrix.py"],
                                env=environment, check=True, capture_output=True, text=True)
        self.assertEqual(json.loads(result.stdout)["count"], "30")
        environment["GITHUB_REF_NAME"] = "v999.0.0"
        result = subprocess.run([sys.executable, ROOT / "scripts/ci/matrix.py"],
                                env=environment, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)


class ReleaseVerificationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.target = {"platform": "windows-2022", "arch": "x86_64", "pg": 17}
        self.matrix = {"include": [self.target]}
        self.manifest = {"format": 1, "extension": "geometry_pyramid", "version": "0.1.1",
                         "postgresql": {"major": 17},
                         "target": {"platform": "windows-2022", "architecture": "x86_64"},
                         "source_commit": "a" * 40}
        self.write_package()

    def write_package(self):
        path = self.directory / release.archive_name("0.1.1", self.target)
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("bundle/manifest.json", json.dumps(self.manifest))
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        path.with_name(path.name + ".sha256").write_text(f"{digest}  {path.name}\n")
        return path

    def test_verified_set_has_machine_readable_index_and_checksums(self):
        index = release.verify(self.directory, self.matrix, "0.1.1")
        self.assertEqual(index["artifact_count"], 1)
        self.assertIn("release-manifest.json", (self.directory / "SHA256SUMS").read_text())

    def test_missing_target_is_release_failure(self):
        self.matrix["include"].append({**self.target, "pg": 18})
        with self.assertRaisesRegex(ValueError, "missing="):
            release.verify(self.directory, self.matrix, "0.1.1")

    def test_corrupted_archive_is_release_failure(self):
        path = self.directory / release.archive_name("0.1.1", self.target)
        with path.open("ab") as stream:
            stream.write(b"tampered")
        with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
            release.verify(self.directory, self.matrix, "0.1.1")

    def test_unverified_extra_files_cannot_be_published(self):
        (self.directory / "unexpected.exe").write_bytes(b"unverified")
        with self.assertRaisesRegex(ValueError, "Unverified extra"):
            release.verify(self.directory, self.matrix, "0.1.1")

    def test_same_wrong_commit_is_rejected_against_workflow(self):
        with self.assertRaisesRegex(ValueError, "workflow's source commit"):
            release.verify(self.directory, self.matrix, "0.1.1", expected_commit="b" * 40)

    def test_label_cannot_mask_wrong_pg_binary_manifest(self):
        self.manifest["postgresql"]["major"] = 18
        self.write_package()
        with self.assertRaisesRegex(ValueError, "Manifest disagrees"):
            release.verify(self.directory, self.matrix, "0.1.1")

    def test_packages_from_different_commits_cannot_publish_together(self):
        self.target = {**self.target, "pg": 18}
        self.matrix["include"].append(self.target)
        self.manifest["postgresql"]["major"] = 18
        self.manifest["source_commit"] = "b" * 40
        self.write_package()
        with self.assertRaisesRegex(ValueError, "one known source commit"):
            release.verify(self.directory, self.matrix, "0.1.1")


if __name__ == "__main__":
    unittest.main()

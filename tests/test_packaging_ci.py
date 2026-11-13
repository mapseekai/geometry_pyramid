"""Release gates must reject incomplete, corrupted and incorrectly labeled packages."""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


matrix = module("package_matrix", "scripts/ci/matrix.py")
release = module("verify_release", "scripts/ci/verify-release.py")
smoke = module("package_smoke", "tests/package_smoke.py")


class UpgradeBaselineTests(unittest.TestCase):
    def test_smoke_exercises_both_released_baselines(self):
        arguments = ["package_smoke.py", "--archive", "candidate.tar.gz",
                     "--legacy-staging", "legacy", "--previous-staging", "previous",
                     "--pg-config", "pg_config"]
        with patch.object(sys, "argv", arguments), patch.object(smoke, "smoke") as run:
            smoke.main()
        self.assertEqual([(call.args[1], call.args[2]) for call in run.call_args_list],
                         [(Path("legacy"), "0.1.0"), (Path("previous"), "0.1.2")])

    def test_all_released_versions_have_a_sql_upgrade_path(self):
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        edges = {}
        for script in (ROOT / "sql").glob("geometry_pyramid--*--*.sql"):
            _, before, after = script.stem.split("--")
            edges.setdefault(before, set()).add(after)
        for baseline in ("0.1.0", "0.1.1", "0.1.2"):
            pending, visited = [baseline], set()
            while pending:
                current = pending.pop()
                if current not in visited:
                    visited.add(current)
                    pending.extend(edges.get(current, set()) - visited)
            self.assertIn(version, visited, f"No upgrade from {baseline} to {version}")


class MatrixTests(unittest.TestCase):
    def setUp(self):
        self.config = json.loads((ROOT / ".github/package-targets.json").read_text())
        self.manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())

    def test_every_declared_pg_has_every_native_platform(self):
        targets = matrix.select_matrix(self.config, self.manifest)["include"]
        self.assertEqual(len(targets), 20)
        self.assertEqual({target["pg"] for target in targets}, {15, 16, 17, 18})
        for pg in range(15, 19):
            self.assertEqual(len([target for target in targets if target["pg"] == pg]), 5)

    def test_single_target_dispatch(self):
        targets = matrix.select_matrix(self.config, self.manifest, "windows-2022-x86_64", "15")["include"]
        self.assertEqual(targets, [{"platform": "windows-2022", "arch": "x86_64", "runner": "windows-2022", "pg": 15}])

    def test_removed_versions_cannot_be_reintroduced_in_matrix_and_cargo(self):
        for pg in (13, 14):
            with self.subTest(pg=pg):
                config, manifest = copy.deepcopy(self.config), copy.deepcopy(self.manifest)
                config["postgres"] = sorted([pg, *config["postgres"]])
                manifest["features"][f"pg{pg}"] = []
                with self.assertRaisesRegex(ValueError, "Only PostgreSQL 15 through 18"):
                    matrix.select_matrix(config, manifest)

    def test_manual_build_rejects_unsupported_pg_before_setup(self):
        for pg in (13, 14, 19):
            with self.subTest(pg=pg):
                environment = dict(os.environ, GP_PG_MAJOR=str(pg))
                environment.pop("PG_CONFIG", None)
                result = subprocess.run([sys.executable, ROOT / "scripts/ci/build.py"],
                                        env=environment, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"Unsupported PostgreSQL major: {pg}", result.stderr)

    @unittest.skipUnless(sys.platform in ("linux", "darwin") and shutil.which("bash"),
                         "native Unix and Bash are required for Unix setup")
    def test_unix_setup_rejects_unsupported_pg_before_environment_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "must-not-be-created"
            for pg in (13, 14, 19):
                with self.subTest(pg=pg):
                    environment = dict(os.environ, GP_PG_MAJOR=str(pg), RUNNER_TEMP=str(destination))
                    result = subprocess.run(["bash", ROOT / "scripts/ci/setup-postgres.sh"],
                                            env=environment, capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"Unsupported PostgreSQL major version: {pg}", result.stderr)
                    self.assertFalse(destination.exists())

    def test_new_cargo_pg_feature_cannot_silently_miss_release(self):
        self.manifest["features"]["pg19"] = []
        with self.assertRaisesRegex(ValueError, "differ from Cargo"):
            matrix.select_matrix(self.config, self.manifest)

    def test_unknown_filters_and_duplicate_platforms_fail(self):
        for platform, postgres in [("windows-2022-arm64", "all"), ("all", "13"), ("all", "14"), ("all", "19")]:
            with self.assertRaises(ValueError):
                matrix.select_matrix(self.config, self.manifest, platform, postgres)
        self.config["platforms"].append(copy.deepcopy(self.config["platforms"][0]))
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            matrix.select_matrix(self.config, self.manifest)

    def test_release_tag_cannot_filter_out_a_failing_platform(self):
        version = self.manifest["package"]["version"]
        environment = dict(os.environ, GITHUB_EVENT_NAME="push", GITHUB_REF_TYPE="tag",
                           GITHUB_REF_NAME=f"v{version}", GP_PLATFORM_FILTER="windows-2022-x86_64",
                           GP_POSTGRES_FILTER="15")
        environment.pop("GITHUB_OUTPUT", None)
        result = subprocess.run([sys.executable, ROOT / "scripts/ci/matrix.py"],
                                env=environment, check=True, capture_output=True, text=True)
        self.assertEqual(json.loads(result.stdout)["count"], "20")
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

    def test_unsupported_pg_target_cannot_be_published(self):
        for pg in (13, 14, 19):
            with self.subTest(pg=pg):
                self.matrix["include"][0] = {**self.target, "pg": pg}
                with self.assertRaisesRegex(ValueError, "PostgreSQL 15 through 18"):
                    release.verify(self.directory, self.matrix, "0.1.1")
                self.assertFalse((self.directory / "release-manifest.json").exists())
                self.assertFalse((self.directory / "SHA256SUMS").exists())

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

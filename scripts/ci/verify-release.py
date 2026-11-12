#!/usr/bin/env python3
"""Reject missing, mislabeled or corrupted artifacts before a release is published."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import tarfile
import zipfile


def archive_name(version, target):
    suffix = ".zip" if target["platform"].startswith("windows-") else ".tar.gz"
    return f"geometry_pyramid-v{version}-pg{target['pg']}-{target['platform']}-{target['arch']}{suffix}"


def read_manifest(path):
    if path.name.endswith(".zip"):
        with zipfile.ZipFile(path) as archive:
            names = [name for name in archive.namelist() if name.endswith("/manifest.json")]
            if len(names) != 1:
                raise ValueError(f"Expected one manifest in {path.name}")
            return json.loads(archive.read(names[0]))
    with tarfile.open(path, "r:gz") as archive:
        members = [member for member in archive.getmembers() if member.isfile() and member.name.endswith("/manifest.json")]
        if len(members) != 1:
            raise ValueError(f"Expected one manifest in {path.name}")
        return json.load(archive.extractfile(members[0]))


def verify(directory, matrix, version, expected_commit=None):
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?", version):
        raise ValueError("Invalid release version")
    targets = matrix["include"]
    expected = {archive_name(version, target): target for target in targets}
    if not expected or len(expected) != len(targets):
        raise ValueError("Empty or duplicate release matrix")
    found = {path.name for path in directory.iterdir() if path.name.endswith((".tar.gz", ".zip"))}
    if found != set(expected):
        raise ValueError(f"Artifact set differs: missing={sorted(set(expected) - found)}, unexpected={sorted(found - set(expected))}")
    allowed = set(expected) | {name + ".sha256" for name in expected} | {"release-manifest.json", "SHA256SUMS"}
    extra = {path.name for path in directory.iterdir()} - allowed
    if extra:
        raise ValueError(f"Unverified extra release files: {sorted(extra)}")
    records = []
    checksums = []
    for name, target in sorted(expected.items()):
        archive = directory / name
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        checksum = f"{digest}  {name}\n"
        if archive.with_name(name + ".sha256").read_text(encoding="ascii") != checksum:
            raise ValueError(f"Checksum mismatch: {name}")
        manifest = read_manifest(archive)
        if manifest.get("format") != 1 or manifest.get("extension") != "geometry_pyramid":
            raise ValueError(f"Unrecognized package manifest: {name}")
        if (manifest["version"], manifest["postgresql"]["major"],
            manifest["target"]["platform"], manifest["target"]["architecture"]) != (
                version, target["pg"], target["platform"], target["arch"]):
            raise ValueError(f"Manifest disagrees with artifact name: {name}")
        checksums.append(checksum)
        records.append({"file": name, "sha256": digest, "bytes": archive.stat().st_size, "manifest": manifest})
    commits = {record["manifest"]["source_commit"] for record in records}
    if len(commits) != 1 or not re.fullmatch(r"[0-9a-f]{40,64}", next(iter(commits))):
        raise ValueError("Release artifacts must be built from one known source commit")
    if expected_commit is not None and commits != {expected_commit}:
        raise ValueError("Release artifacts differ from the workflow's source commit")
    index = {"extension": "geometry_pyramid", "version": version,
             "source_commit": next(iter(commits)), "artifact_count": len(records), "artifacts": records}
    index_path = directory / "release-manifest.json"
    index_path.write_text(json.dumps(index, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    index_digest = hashlib.sha256(index_path.read_bytes()).hexdigest()
    checksums.append(f"{index_digest}  release-manifest.json\n")
    (directory / "SHA256SUMS").write_text("".join(checksums), encoding="ascii")
    return index


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    index = verify(args.directory, json.loads(os.environ["GP_EXPECTED_MATRIX"]),
                   os.environ["GP_EXTENSION_VERSION"], os.environ["GP_EXPECTED_COMMIT"])
    print(f"Verified {index['artifact_count']} packages for geometry_pyramid {index['version']}")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(f"### Complete release set: {index['artifact_count']} packages\n\n")
            stream.write("| PostgreSQL | Platform | Architecture | Package |\n| --- | --- | --- | --- |\n")
            for item in index["artifacts"]:
                manifest = item["manifest"]
                stream.write(f"| {manifest['postgresql']['major']} | {manifest['target']['platform']} | "
                             f"{manifest['target']['architecture']} | `{item['file']}` |\n")


if __name__ == "__main__":
    main()

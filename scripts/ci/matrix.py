#!/usr/bin/env python3
"""Select release targets and keep the package matrix aligned with Cargo features."""

import json
import os
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def select_matrix(config, manifest, platform_filter="all", postgres_filter="all"):
    declared = sorted(int(name[2:]) for name in manifest["features"] if re.fullmatch(r"pg\d+", name))
    if config["postgres"] != declared:
        raise ValueError(f"Packaging PostgreSQL versions {config['postgres']} differ from Cargo features {declared}")
    platforms = config["platforms"]
    keys = [f"{item['platform']}-{item['arch']}" for item in platforms]
    if len(keys) != len(set(keys)):
        raise ValueError("Duplicate platform/architecture in package-targets.json")
    if platform_filter != "all" and platform_filter not in keys:
        raise ValueError(f"Unknown platform filter: {platform_filter}")
    if postgres_filter != "all" and postgres_filter not in [str(v) for v in declared]:
        raise ValueError(f"Unknown PostgreSQL filter: {postgres_filter}")
    return {"include": [
        {**item, "pg": pg}
        for pg in declared
        for item in platforms
        if (platform_filter == "all" or f"{item['platform']}-{item['arch']}" == platform_filter)
        and (postgres_filter == "all" or str(pg) == postgres_filter)
    ]}


def package_version(manifest):
    version = manifest["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?", version):
        raise ValueError("Unsafe or invalid extension version")
    pgrx = manifest["dependencies"]["pgrx"]
    tests = manifest["dev-dependencies"]["pgrx-tests"]
    if not isinstance(pgrx, str) or not re.fullmatch(r"=\d+\.\d+\.\d+", pgrx) or pgrx != tests:
        raise ValueError("pgrx and pgrx-tests must have the same exact version pin")
    return version, pgrx[1:]


def main():
    config = json.loads((ROOT / ".github/package-targets.json").read_text())
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    version, pgrx = package_version(manifest)
    event = os.environ.get("GITHUB_EVENT_NAME", "workflow_dispatch")
    platform_filter = os.environ.get("GP_PLATFORM_FILTER", "all") if event == "workflow_dispatch" else "all"
    postgres_filter = os.environ.get("GP_POSTGRES_FILTER", "all") if event == "workflow_dispatch" else "all"
    matrix = select_matrix(config, manifest, platform_filter, postgres_filter)
    tag = f"v{version}"
    if event == "push" and os.environ.get("GITHUB_REF_TYPE") == "tag":
        if os.environ.get("GITHUB_REF_NAME") != tag:
            raise ValueError(f"Release tag must match Cargo.toml: expected {tag}")
    output = {
        "matrix": json.dumps(matrix, separators=(",", ":")),
        "version": version,
        "tag": tag,
        "pgrx": pgrx,
        "count": str(len(matrix["include"])),
    }
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            for key, value in output.items():
                stream.write(f"{key}={value}\n")
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()

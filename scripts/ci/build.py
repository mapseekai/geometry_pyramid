#!/usr/bin/env python3
"""Build each candidate and its genuine upgrade baseline in the selected environment."""

import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]


def run(arguments, cwd=ROOT):
    print("+", " ".join(map(str, arguments)), flush=True)
    subprocess.run(list(map(str, arguments)), cwd=cwd, check=True)


def main():
    major = os.environ["GP_PG_MAJOR"]
    if major not in {str(v) for v in range(13, 19)}:
        raise ValueError(f"Unsupported PostgreSQL major: {major}")
    pg_config = Path(os.environ["PG_CONFIG"]).resolve()
    os.environ["CARGO_TARGET_DIR"] = str(ROOT / "target")
    feature_args = ["--no-default-features", "--features", f"pg{major}"]
    run(["cargo", "pgrx", "test", f"pg{major}", *feature_args])
    run(["cargo", "pgrx", "package", "--pg-config", pg_config, *feature_args,
         "--out-dir", ROOT / "package"])
    run(["cargo", "pgrx", "package", "--pg-config", pg_config, *feature_args,
         "--out-dir", ROOT / "legacy-package"], cwd=ROOT / "upgrade-baseline")
    run(["cargo", "pgrx", "package", "--pg-config", pg_config, *feature_args,
         "--out-dir", ROOT / "previous-package"], cwd=ROOT / "upgrade-previous")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    run([sys.executable, ROOT / "scripts/package.py", "--staging", ROOT / "package",
         "--pg-config", pg_config, "--platform", os.environ["GP_PLATFORM"],
         "--arch", os.environ["GP_ARCH"], "--out-dir", ROOT / "dist", "--commit", commit])
    archives = [p for p in (ROOT / "dist").iterdir() if p.name.endswith((".tar.gz", ".zip"))]
    if len(archives) != 1:
        raise ValueError(f"Expected exactly one package archive, found {len(archives)}")
    run([sys.executable, ROOT / "tests/package_smoke.py", "--archive", archives[0],
         "--legacy-staging", ROOT / "legacy-package",
         "--previous-staging", ROOT / "previous-package", "--pg-config", pg_config])
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(f"### Verified `{archives[0].name}`\n\n")
            stream.write("Rust/PostGIS regressions, packaged installation, real v0.1.0 and v0.1.1 upgrades, "
                         "and separate-session permission checks passed.\n")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Verify a real release bundle in a disposable PostgreSQL installation.

The selected PostgreSQL installation must be dedicated to this test: its extension
files are replaced first by the v0.1.0 baseline and then by the candidate bundle.
Database data is created in a separate temporary cluster and removed afterwards.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import subprocess
import tarfile
import tempfile
import zipfile

TESTS = Path(__file__).resolve().parent


def command(args, env=None):
    print("+", " ".join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), env=env, check=True)


def pg_value(pg_config, option):
    return subprocess.check_output([str(pg_config), option], text=True).strip()


def unpack(archive, destination):
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as source:
            for name in source.namelist():
                relative = Path(name)
                if relative.is_absolute() or ".." in relative.parts or "\\" in name:
                    raise ValueError("Unsafe archive member")
            source.extractall(destination)
    else:
        with tarfile.open(archive, "r:gz") as source:
            source.extractall(destination, filter="data")
    roots = list(destination.iterdir())
    if len(roots) != 1 or not roots[0].is_dir():
        raise ValueError("Release bundle must contain one top-level directory")
    return roots[0]


def atomic_copy(source, destination):
    descriptor, name = tempfile.mkstemp(prefix=".gp-smoke-", dir=destination.parent)
    os.close(descriptor)
    temporary = Path(name)
    try:
        shutil.copyfile(source, temporary)
        temporary.chmod(0o644)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def install_legacy(staging, library_dir, extension_dir, suffix):
    if os.name == "nt":
        source_library = staging / "lib"
        source_extensions = staging / "share/extension"
    else:
        source_library = staging / str(library_dir).lstrip("/")
        source_extensions = staging / str(extension_dir).lstrip("/")
    # Remove candidate artifacts left by cargo pgrx test. Fresh/upgrade checks
    # must see only files supplied by the packages being tested.
    for path in extension_dir.glob("geometry_pyramid*"):
        if path.is_file():
            path.unlink()
    atomic_copy(source_library / f"geometry_pyramid{suffix}", library_dir / f"geometry_pyramid{suffix}")
    for path in sorted(source_extensions.glob("geometry_pyramid*")):
        if path.is_file():
            atomic_copy(path, extension_dir / path.name)
    if not (extension_dir / "geometry_pyramid--0.1.0.sql").is_file():
        raise ValueError("The baseline package must contain the genuine 0.1.0 install SQL")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--legacy-staging", type=Path, required=True)
    parser.add_argument("--pg-config", type=Path, required=True)
    args = parser.parse_args()
    pg_config = args.pg_config.resolve()
    bindir = Path(pg_value(pg_config, "--bindir"))
    libdir = Path(pg_value(pg_config, "--pkglibdir"))
    extdir = Path(pg_value(pg_config, "--sharedir")) / "extension"
    version = pg_value(pg_config, "--version")
    major_match = re.match(r"PostgreSQL\s+(\d+)", version)
    if not major_match:
        raise ValueError(f"Unrecognized PostgreSQL version: {version}")
    major = int(major_match.group(1))
    windows = os.name == "nt"
    executable_suffix = ".exe" if windows else ""
    executable = lambda name: bindir / (name + executable_suffix)
    suffix = ".dll" if windows else (".dylib" if os.uname().sysname == "Darwin" and major >= 16 else ".so")
    # A short Unix socket path also works within macOS's 104-byte limit.
    with tempfile.TemporaryDirectory(prefix="gp-smoke-", dir=None if windows else "/tmp") as temporary:
        cluster = Path(temporary)
        bundle_dir = cluster / "bundle"
        bundle_dir.mkdir()
        bundle = unpack(args.archive.resolve(), bundle_dir)
        manifest = json.loads((bundle / "manifest.json").read_text())
        if manifest["postgresql"]["major"] != major:
            raise ValueError("Bundle and smoke-test PostgreSQL major differ")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        environment = dict(os.environ)
        environment.update(PGHOST="127.0.0.1" if windows else str(cluster),
                           PGPORT=str(port), PGDATABASE="postgres", PGUSER="gp_package_admin")
        data = cluster / "data"
        log = cluster / "server.log"
        command([executable("initdb"), "-D", data, "--auth=trust", "--no-locale", "-E", "UTF8",
                 "--username=gp_package_admin"], environment)
        options = ["-p", str(port), "-c", "jit=off", "-c", "max_worker_processes=32"]
        if windows:
            options += ["-h", "127.0.0.1"]
            server_options = subprocess.list2cmdline(options)
        else:
            options += ["-k", str(cluster), "-c", "listen_addresses="]
            server_options = shlex.join(options)
        started = False
        try:
            command([executable("pg_ctl"), "-D", data, "-l", log, "-o", server_options, "-w", "start"], environment)
            started = True
            command([executable("createdb"), "gp_upgrade"], environment)
            install_legacy(args.legacy_staging.resolve(), libdir, extdir, suffix)
            psql = [executable("psql"), "-X", "-v", "ON_ERROR_STOP=1"]
            command([*psql, "-d", "gp_upgrade", "-v", "seed=true", "-f", TESTS / "package_upgrade.sql"], environment)
            # Every psql process has exited before replacing the native library.
            # Windows additionally locks mapped DLLs until backend exit; stop
            # this private cluster so backend shutdown cannot race the installer.
            if windows:
                command([executable("pg_ctl"), "-D", data, "-m", "fast", "-w", "stop"], environment)
                started = False
                command(["pwsh", "-NoProfile", "-File", bundle / "install.ps1", "-PgConfig", pg_config], environment)
                command([executable("pg_ctl"), "-D", data, "-l", log, "-o", server_options, "-w", "start"], environment)
                started = True
            else:
                command(["bash", bundle / "install.sh", "--pg-config", pg_config], environment)
            command([*psql, "-d", "gp_upgrade", "-v", "seed=false", "-f", TESTS / "package_upgrade.sql"], environment)
            command([*psql, "-c", "CREATE EXTENSION postgis; CREATE EXTENSION geometry_pyramid;"], environment)
            installed = subprocess.check_output(
                [str(arg) for arg in [*psql, "-At", "-c", "SELECT geometry_pyramid_version()"]],
                env=environment, text=True).strip()
            if installed != manifest["version"]:
                raise ValueError(f"Loaded extension version {installed} differs from bundle {manifest['version']}")
            command([*psql, "-f", TESTS / "session_safety.sql"], environment)
        except BaseException:
            if log.exists():
                print(log.read_text(encoding="utf-8", errors="replace")[-16000:])
            raise
        finally:
            if started or (data / "postmaster.pid").exists():
                command([executable("pg_ctl"), "-D", data, "-m", "fast", "-w", "stop"], environment)
    print(f"Packaged installation and v0.1.0 upgrade passed: {args.archive.name}")


if __name__ == "__main__":
    main()

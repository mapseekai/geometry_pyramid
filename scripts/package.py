#!/usr/bin/env python3
"""Turn cargo-pgrx's installation tree into a relocatable release bundle."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile


EXTENSION = "geometry_pyramid"
REPO = Path(__file__).resolve().parent.parent
ARCHITECTURES = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}


def pg_value(pg_config: str, option: str) -> str:
    return subprocess.check_output([pg_config, option], text=True).strip()


def binary_target(path: Path) -> tuple[str, str]:
    """Read the executable header; never label a binary from a CLI flag alone."""
    data = path.read_bytes()
    if data[:4] == b"\x7fELF" and len(data) >= 20:
        if data[4] != 2 or data[5] not in (1, 2):
            raise ValueError("only 64-bit ELF artifacts are supported")
        endian = "<" if data[5] == 1 else ">"
        machine = struct.unpack_from(endian + "H", data, 18)[0]
        architecture = {62: "x86_64", 183: "aarch64"}.get(machine)
        system = "linux"
    elif data[:4] in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf") and len(data) >= 8:
        endian = "<" if data[:4] == b"\xcf\xfa\xed\xfe" else ">"
        machine = struct.unpack_from(endian + "I", data, 4)[0]
        architecture = {0x01000007: "x86_64", 0x0100000C: "aarch64"}.get(machine)
        system = "macos"
    elif data[:2] == b"MZ" and len(data) >= 64:
        offset = struct.unpack_from("<I", data, 60)[0]
        if offset + 24 > len(data) or data[offset : offset + 4] != b"PE\0\0":
            raise ValueError("invalid PE artifact")
        machine = struct.unpack_from("<H", data, offset + 4)[0]
        architecture = {0x8664: "x86_64", 0xAA64: "aarch64"}.get(machine)
        system = "windows"
    else:
        raise ValueError(f"unrecognized binary format: {path.name}")
    if architecture is None:
        raise ValueError(f"unsupported binary machine: {machine}")
    return system, architecture


def platform_system(platform: str) -> str:
    if platform.startswith("windows-"):
        return "windows"
    if platform.startswith("macos-"):
        return "macos"
    if platform.startswith(("ubuntu-", "debian-", "rocky-", "rhel-", "almalinux-", "linux-")):
        return "linux"
    raise ValueError("platform must identify a Linux distribution, macos-<release>, or windows-<release>")


def regular_file(path: Path) -> Path:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"expected a regular artifact file: {path}")
    return path


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_identity(commit: str | None) -> tuple[str, int]:
    if commit is not None and not re.fullmatch(r"[0-9a-f]{40,64}", commit):
        raise ValueError("--commit must be a complete hexadecimal Git commit ID")
    try:
        if commit is None:
            commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip()
        default_epoch = subprocess.check_output(["git", "show", "-s", "--format=%ct", commit], cwd=REPO, text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        commit = commit or "unknown"
        default_epoch = "0"
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", default_epoch))
    if epoch < 0:
        raise ValueError("SOURCE_DATE_EPOCH must be nonnegative")
    return commit, epoch


def make_bundle(args: argparse.Namespace) -> Path:
    if not re.fullmatch(r"[a-z0-9]+(?:[.-][a-z0-9]+)*", args.platform):
        raise ValueError("--platform must be a lowercase platform label, for example ubuntu-24.04")
    system = platform_system(args.platform)
    architecture = ARCHITECTURES[args.arch]
    postgres_version = pg_value(args.pg_config, "--version")
    match = re.match(r"PostgreSQL\s+(\d+)(?:\.|\b)", postgres_version)
    if not match or int(match.group(1)) not in range(15, 19):
        raise ValueError(f"only PostgreSQL 15 through 18 are supported: {postgres_version}")
    major = int(match.group(1))
    if system == "windows":
        libdir = args.staging / "lib"
        extdir = args.staging / "share" / "extension"
        suffix = ".dll"
    else:
        library_path = pg_value(args.pg_config, "--pkglibdir")
        share_path = pg_value(args.pg_config, "--sharedir")
        if not library_path.startswith("/") or not share_path.startswith("/"):
            raise ValueError("pg_config must return absolute installation paths")
        libdir = args.staging / library_path.lstrip("/")
        extdir = args.staging / share_path.lstrip("/") / "extension"
        suffix = ".dylib" if system == "macos" and major >= 16 else ".so"
    library = regular_file(libdir / (EXTENSION + suffix))
    actual_system, actual_architecture = binary_target(library)
    if (actual_system, actual_architecture) != (system, architecture):
        raise ValueError(f"binary is {actual_system}/{actual_architecture}, requested {system}/{architecture}")
    control = regular_file(extdir / (EXTENSION + ".control"))
    control_text = control.read_text(encoding="utf-8")
    version_match = re.search(r"^\s*default_version\s*=\s*'([A-Za-z0-9][A-Za-z0-9.+-]*)'\s*(?:#.*)?$", control_text, re.MULTILINE)
    if not version_match:
        raise ValueError("packaged control must contain a resolved default_version")
    version = version_match.group(1)
    regular_file(extdir / f"{EXTENSION}--{version}.sql")
    sql_files = sorted(extdir.glob(f"{EXTENSION}--*.sql"))
    for path in sql_files:
        regular_file(path)
        if not re.fullmatch(r"geometry_pyramid--[A-Za-z0-9.+-]+\.sql", path.name):
            raise ValueError(f"unexpected SQL filename: {path.name}")
    glibc_versions = re.findall(rb"GLIBC_(\d+\.\d+(?:\.\d+)?)\x00", library.read_bytes()) if system == "linux" else []
    minimum_glibc = max((v.decode() for v in glibc_versions), key=lambda v: tuple(map(int, v.split("."))), default="none")
    commit, epoch = source_identity(args.commit)
    name = f"{EXTENSION}-v{version}-pg{major}-{args.platform}-{architecture}"
    args.out_dir.mkdir(parents=True, exist_ok=True)
    archive = args.out_dir / (name + (".zip" if system == "windows" else ".tar.gz"))
    with tempfile.TemporaryDirectory(prefix="gp-bundle-") as temporary:
        root = Path(temporary) / name
        (root / "lib").mkdir(parents=True)
        (root / "extension").mkdir()
        shutil.copyfile(library, root / "lib" / library.name)
        for path in [control, *sql_files]:
            shutil.copyfile(path, root / "extension" / path.name)
        installer = "install.ps1" if system == "windows" else "install.sh"
        shutil.copyfile(REPO / "scripts" / installer, root / installer)
        if system != "windows":
            (root / installer).chmod(0o755)
        shutil.copyfile(REPO / "LICENSE", root / "LICENSE")
        metadata = {
            "format": "1", "extension": EXTENSION, "version": version,
            "postgres_major": str(major), "system": system,
            "architecture": architecture, "platform": args.platform,
            "library": library.name, "minimum_glibc": minimum_glibc,
        }
        (root / "package.metadata").write_text("".join(f"{key}={value}\n" for key, value in metadata.items()), encoding="ascii")
        manifest = {
            "format": 1, "extension": EXTENSION, "version": version,
            "postgresql": {"major": major, "build_version": postgres_version},
            "target": {"system": system, "platform": args.platform, "architecture": architecture, "minimum_glibc": None if minimum_glibc == "none" else minimum_glibc},
            "source_commit": commit, "source_date_epoch": epoch,
            "requires_extensions": ["postgis"], "library": "lib/" + library.name,
            "installer": installer,
        }
        (root / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        install_command = f'pwsh -File .\\install.ps1 -PgConfig "C:\\Program Files\\PostgreSQL\\{major}\\bin\\pg_config.exe"' if system == "windows" else f'sudo ./install.sh --pg-config /path/to/postgresql-{major}/bin/pg_config'
        (root / "INSTALL.txt").write_text(
            f"{EXTENSION} {version} / PostgreSQL {major} / {args.platform} / {architecture}\n\n"
            f"Install into the PostgreSQL server using:\n  {install_command}\n\n"
            "Install PostGIS for the same PostgreSQL installation first.\n"
            "The platform label identifies the build environment, not a promise of compatibility with other distributions.\n"
            "For an upgrade, stop active extension work and reconnect database sessions after replacing files.\n"
            "On Windows, stop PostgreSQL services using this installation before replacing a loaded DLL.\n"
            "The installer copies files only. Enable or upgrade the extension in each intended database using an administrator.\n"
            "See https://github.com/mapseekai/geometry_pyramid for installation and version-specific upgrade instructions.\n",
            encoding="utf-8",
        )
        files = sorted(path for path in root.rglob("*") if path.is_file())
        (root / "SHA256SUMS").write_text("".join(f"{sha256(path)}  {path.relative_to(root).as_posix()}\n" for path in files), encoding="ascii")
        if system == "windows":
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
                # ZIP timestamps start at 1980. Fixed metadata makes reruns reproducible.
                for path in sorted(root.rglob("*")):
                    if path.is_file():
                        info = zipfile.ZipInfo(path.relative_to(root.parent).as_posix(), (1980, 1, 1, 0, 0, 0))
                        info.compress_type = zipfile.ZIP_DEFLATED
                        info.external_attr = 0o100644 << 16
                        output.writestr(info, path.read_bytes())
        else:
            with archive.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as output:
                    for path in [root, *sorted(root.rglob("*"))]:
                        info = output.gettarinfo(str(path), arcname=path.relative_to(root.parent).as_posix())
                        info.uid = info.gid = 0
                        info.uname = info.gname = "root"
                        info.mtime = epoch
                        info.mode = 0o755 if path.is_dir() or path.name == installer else 0o644
                        if path.is_file():
                            with path.open("rb") as content:
                                output.addfile(info, content)
                        else:
                            output.addfile(info)
    archive.with_name(archive.name + ".sha256").write_text(f"{sha256(archive)}  {archive.name}\n", encoding="ascii")
    return archive


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staging", required=True, type=Path)
    parser.add_argument("--pg-config", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True, choices=sorted(ARCHITECTURES))
    parser.add_argument("--out-dir", required=True, type=Path)
    parser.add_argument("--commit")
    args = parser.parse_args()
    try:
        archive = make_bundle(args)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"Packaging failed: {error}", file=sys.stderr)
        return 1
    print(archive)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
